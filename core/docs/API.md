# harness-core — Plugin Harness API

`harness-core` is a Rust library for building applications out of **plugins**: isolated
units that declare what they provide, what they need, and which events they touch.
A central `Context` wires plugins together via string-keyed **service injection** and a
typed **event bus**, tracks dependencies in a graph, and supports atomic load/unload at
runtime.

- **Crate:** `harness-core` (`core/Cargo.toml`)
- **Modules:** `error`, `event`, `key`, `plugin` are public; `context`, `graph`, `service` are internal
- **Re-exports:** `Context`, `LoadOutcome`, `Error`, `Result`, `Key`, `Plugin`, `PluginMeta`, `Event`, `Events`, `BoxedEvent`, `Handler`, `HandlerFuture`, `WaterfallHandler`

```toml
[dependencies]
harness-core = { path = "../core" }
```

## Feature flags

| Feature     | Default | Effect                                                                 |
| ----------- | ------- | ---------------------------------------------------------------------- |
| `rt-tokio`  | yes     | Enables `emit_key_detached` / `Events::emit_detached` (tokio spawning) |

Everything else works without any async runtime; sync-only usage is supported and tested.

---

## Mental model

```
            ┌────────────────────────────────────────────┐
            │                  Context                   │
            │  services: Key -> Arc<dyn Any + Send+Sync> │
            │  events:   Key -> Channel { ty, listeners }│
            │  graph:    active + pending + build state  │
            └────────────────────────────────────────────┘
               ▲ provide/inject      ▲ listen/emit/waterfall
        ┌──────┴──────┐         ┌─────┴──────┐
        │   Plugin A  │         │  Plugin B  │
        └─────────────┘         └────────────┘
```

A plugin is defined by two things:

1. **`meta()`** — declarative metadata: name, provided service keys, injected service keys,
   event channels it emits on or listens to (with payload types).
2. **`build(ctx)`** — the one-shot activation hook: register services and listeners.

The harness uses `meta()` for validation and ordering *before* `build()` ever runs:

- **Services** are looked up by string [`Key`]. A plugin that declares `injects` for a key no
  one provides yet is **parked** as pending and auto-activated once the key appears.
- **Events** flow over named channels. Each channel is bound to exactly one payload type;
  every listener on it sees the same type. Declarations in `meta()` are checked for
  cross-plugin consistency at load time.
- **Unloading** removes the target plus every transitive dependent, in dependency order,
  and purges their services and listeners atomically.

---

## Quick start

```rust
use std::sync::Arc;
use harness_core::{Context, Plugin, PluginMeta};

struct Greeting(String);
struct Tick(u32);

struct GreetingPlugin;

impl Plugin for GreetingPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("greeting").provides("app.greeting")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key("app.greeting", Arc::new(Greeting("hello".into())));
        Ok(())
    }
}

struct StatsPlugin;

impl Plugin for StatsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("stats").listens::<Tick>("app.tick")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.on_key::<Tick, _, _>("app.tick", |t| async move {
            println!("tick {}", t.0);
        })?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> harness_core::Result<()> {
    let ctx = Context::root();

    // Load order doesn't matter here: stats has no injected services,
    // so both plugins activate immediately.
    ctx.load(StatsPlugin)?;
    ctx.load(GreetingPlugin)?;

    // Typed handle to a channel; emit and drive async handlers ourselves.
    let ticks = ctx.events::<Tick>("app.tick");
    for fut in ticks.emit(Tick(2))? {
        fut.await;
    }

    assert_eq!(ctx.plugin_names(), vec!["stats", "greeting"]);
    Ok(())
}
```

The only way to obtain an `Events<E>` handle is `Context::events` — construction is
crate-private, so handles always stay attached to a live `Context`.

> **Tip — shareable bus handles:** store an `Arc<Events<E>>` as a service so other code can
> emit without holding a `Context`:
>
> ```rust
> fn build(&self, ctx: Context) -> Result<()> {
>     let ticks = ctx.events::<Tick>("app.tick");
>     ctx.provide_key("app.clock", Arc::new(ticks));
>     Ok(())
> }
> ```

---

# API reference

## `trait Plugin`

```rust
pub trait Plugin: Send + Sync + 'static {
    fn meta(&self) -> PluginMeta;
    fn build(&self, ctx: Context) -> Result<()>;
}
```

| Method   | Called                              | Purpose                                                        |
| -------- | ----------------------------------- | -------------------------------------------------------------- |
| `meta()` | Before activation (may be called repeatedly) | Declare identity, services, event channels             |
| `build()`| Once per successful reservation      | Wire up: `provide_key`, `inject_key`, `on_key`, `on_sync_key`, `events` |

**Bounds:** `Send + Sync + 'static` — plugins may be shared across threads and stored in
`Arc<dyn Plugin>` (see `load_dyn`).

**`build()` contract:**

- Runs synchronously, outside the internal locks, wrapped in `catch_unwind`.
  - `Ok(())` → plugin promoted to *active*.
  - `Err(_)` or panic → full rollback of the reservation; nothing leaks into the registry
    (panic surfaces as `Error::PluginPanicked`).
- May nest-load child plugins via `ctx.load(...)` / `ctx.load_dyn(...)`.
- Treat it as **"register and return"**: provide services, inject dependencies, attach
  listeners. Long-running work belongs in listeners or tasks spawned from them.

### Why is `build()` synchronous?

By design. Async initialization would force boxed futures on every implementor (the trait
is dyn-compatible), weaken panic containment across await points, and require an executor
for even trivial plugins. Async work belongs in event handlers — which are fully async —
or in tasks spawned from `build()`. If a first-class async init phase is ever needed, it
can be added as a separate additive hook without breaking existing plugins.

## `struct PluginMeta`

Declarative builder describing the plugin's interface.

```rust
PluginMeta::new("name")
    .provides("svc.a")              // service keys this plugin registers
    .injects("svc.b")               // service keys required before activation
    .emits::<MyEvent>("ch.tick")    // channels emitted (with payload type)
    .listens::<MyEvent>("ch.tick")  // channels listened to (same type rule)
    .waterfalls::<MyEvent>("ch.cfg") // channels with waterfall (transform) handlers
```

| Method                          | Notes                                                            |
| ------------------------------- | ---------------------------------------------------------------- |
| `new(name)`                     | Names must be unique among active + pending plugins              |
| `provides(key)`                 | Conflicts with another provider → `Error::ServiceConflict`; a plugin may not provide a key it also injects (`SelfDependency`) |
| `injects(key)`                  | Missing at load time → plugin parks (`LoadOutcome::Pending`)     |
| `emits::<E>(key)` / `listens::<E>(key)` / `waterfalls::<E>(key)` | Same channel must agree on one payload type everywhere; conflicts → `SelfEventConflict` (own meta), `EventDeclConflict` (other plugins), `EventChannelMismatch` (live channel) |
| `name()`, `emits_of()`, `listens_of()`, `waterfalls_of()` | Accessors                                                 |

## `struct Key`

Cheap, cloneable string key (`Arc<str>`). `From<&str>` and `From<String>`; `"literal"`
works anywhere a `Key` is expected. Dotted namespaces (`"app.greeting"`) are convention,
not enforced.

## `enum Event` and `struct Events<E>`

`Event` is blanket-implemented for every `Any + Send + Sync` type — just define a plain struct:

```rust
struct Tick(u32);   // automatically an Event
```

Typed channel handle obtained from `ctx.events::<E>(key)` (or stored as a service):

| Method                       | Description                                                                    |
| ---------------------------- | ------------------------------------------------------------------------------ |
| `on(handler) -> Result<()>`  | Register an **async** listener `Fn(Arc<E>) -> impl Future<Output=()> + Send`   |
| `emit(event) -> Result<Vec<HandlerFuture>>` | Emit; returns un-awaited futures of async listeners              |
| `emit_detached(event) -> Result<()>` | *(rt-tokio)* `tokio::spawn`s each returned future — fire-and-forget    |
| `on_waterfall(handler) -> Result<()>` | Register a **waterfall** handler `Fn(Arc<E>) -> impl Future<Output=E> + Send` |
| `waterfall(event) -> Result<E>` | *(async)* Chain waterfall handlers sequentially; returns final transformed event |

For raw access see `Context::on_key` / `on_sync_key` / `emit_key` / `on_waterfall_key` / `waterfall_key`.

### Emit semantics

Documented at `Context::emit_key` (`src/context.rs`):

- The listener list is **snapshotted under a read lock, then released** before any handler
  runs — handlers may freely register/unregister listeners (even on this channel) without
  deadlock.
- **Sync listeners run inline** on the calling thread before `emit` returns; a slow sync
  handler stalls the emitter and every later listener.
- **Async listeners are not awaited** by emit. Their futures are returned so *you* choose:
  - `for fut in futs { fut.await }` — sequential completion
  - `futures::future::join_all(futs)` — concurrent completion
  - `emit_detached` — spawn each onto the tokio runtime
- Emitting on a channel with zero listeners — including one never created — is a silent
  no-op returning `Ok(vec![])`.
- Wrong payload type for the channel → `Error::PayloadTypeMismatch`.

### Waterfall semantics

A **waterfall** chains listeners sequentially, where each listener receives the event and
can **modify it** before passing it to the next. This is in contrast to `emit`, which fans
out the same event to all listeners independently.

```rust
let bus = ctx.events::<Config>("app.config");
bus.on_waterfall(|cfg| async move { Config { debug: true, ..(*cfg) } }).unwrap();
bus.on_waterfall(|cfg| async move { Config { max_retries: 5, ..(*cfg) } }).unwrap();
let final_cfg = bus.waterfall(Config::default()).await?;
// final_cfg.debug == true, final_cfg.max_retries == 5
```

**Key properties:**

- **Sequential execution:** handlers run in registration order, each awaiting the next.
- **Transform:** each handler receives `Arc<E>` and returns `E` (the modified event).
- **Return value:** `waterfall` returns the final transformed event after all handlers run.
- **Independent of emit:** waterfall handlers on a channel are *not* triggered by `emit`,
  and regular listeners are *not* triggered by `waterfall`. The two dispatch modes coexist
  on the same channel but remain cleanly separated.
- **No handlers:** if no waterfall listeners are registered, the original event is returned
  unchanged.
- **Type safety:** the event type must match the channel's registered type; mismatch →
  `Error::PayloadTypeMismatch`.
- **Clone required:** `E` must implement `Clone` because the final `Arc<E>` may need to be
  unwrapped.

## `struct Context`

Cheaply cloneable handle (`Arc` internally) tagged with an owner (`System` or
`Plugin(name)`), used for attributing and purging listeners/services on unload. All
methods take `&self` and are safe to call from any thread.

### Service registry (DI)

| Method | Description |
| ------ | ----------- |
| `provide_key<T: Send + Sync + 'static>(key, Arc<T>)` | Register a service. Overwrites silently at runtime; *declarations* are conflict-checked at load time |
| `inject_key<T>(key) -> Result<Arc<T>>` | Fetch a typed shared handle; missing or wrong type → `MissingService` |
| `try_inject_key<T>(key) -> Option<Arc<T>>` | Non-failing variant |

### Event registration

| Method | Description |
| ------ | ----------- |
| `on_key<E, F, Fut>(key, handler)` | Async listener; handler receives `Arc<E>` |
| `on_sync_key<E, F>(key, handler)` | Sync listener; handler receives `&E`, runs inline on emit |
| `on_waterfall_key<E, F, Fut>(key, handler)` | Waterfall handler; receives `Arc<E>`, returns transformed `E` |
| `events::<E>(key) -> Events<E>` | Typed reusable handle (`on` / `emit` / `emit_detached` / `on_waterfall` / `waterfall`) |

Listeners registered through a plugin's context are owned by that plugin and removed when
it unloads.

### Emitting

| Method | Description |
| ------ | ----------- |
| `emit_key<E>(key, event) -> Result<Vec<HandlerFuture>>` | See [Emit semantics](#emit-semantics) |
| `emit_key_detached<E>(key, event) -> Result<()>` | *(rt-tokio)* spawn each async handler |
| `waterfall_key<E>(key, event) -> Result<E>` | *(async)* See [Waterfall semantics](#waterfall-semantics) |

### Lifecycle

| Method | Description |
| ------ | ----------- |
| `load<P: Plugin>(plugin) -> Result<LoadOutcome>` | Validate → reserve → `build()` → cascade pending |
| `load_dyn(Arc<dyn Plugin>) -> Result<LoadOutcome>` | Object-safe variant |
| `unload(name) -> Result<Vec<String>>` | Tear down `name` plus transitive dependents; returns the dependents removed, in teardown order (target implied last) |

**Load pipeline** (`src/context.rs`):

1. `validate_meta` against the graph, service registry, and live event channels (all
   conflicts listed under `PluginMeta`).
2. Any injected key missing → plugin is **parked**; returns
   `LoadOutcome::Pending { missing }`. Nothing else happens.
3. Otherwise the declaration is **reserved** (provides/emits/listens recorded, plugin
   marked *building*), then `build()` runs outside the locks.
4. On success the plugin is promoted; on error/panic everything rolls back.
5. `activate_pending` then loops: any parked plugin whose deps are now satisfied gets
   validated + built too (cascade). Among simultaneously-ready candidates, first activated
   wins; losers surface their conflict error.

**Unload rules:**

- Unknown name → `UnknownPlugin`.
- Unloading a still-pending plugin simply cancels the park (returns empty vec).
- Refused with `Error::PluginBusy` while the target or any of its dependents is mid-build.
- Teardown order is computed by Kahn's algorithm over the affected sub-graph: a dependent
  is removed only after everything that depends on *it* within the affected set; ties
  break by lowest load order.
- Removal purges each plugin's provided services and its owned listeners/event
  declarations. Channels left with no listeners are dropped.
- In-flight detached handlers (`emit_detached` already spawned) are **not** cancelled.

### Introspection

| Method | Returns |
| ------ | ------- |
| `plugin_names()` | Active plugins in load order |
| `pending_names()` | Parked plugins awaiting dependencies |
| `provider_of(&key)` | Name of the plugin providing a service key |
| `emitters_of(&key)` / `listeners_of(&key)` / `waterfallers_of(&key)` | Plugins declaring emit/listen/waterfall on a channel |

## `enum LoadOutcome`

```rust
pub enum LoadOutcome {
    Activated,
    Pending { missing: Vec<Key> },
}
```

Both are non-errors — pending just means "parked until someone provides the missing keys".

## `enum Error` (`harness_core::error`)

| Variant | Raised when |
| ------- | ----------- |
| `MissingService(Key)` | `inject_key` for unregistered key (or wrong stored type) |
| `DuplicatePlugin(String)` | Load with an already active/pending name |
| `UnknownPlugin(String)` | Unload of a name that isn't loaded |
| `ServiceConflict { key, provider }` | Two plugins declare `provides` for the same key |
| `EventConflict { key }` | Listener registered on a channel bound to a different payload type |
| `PayloadTypeMismatch { key }` | Emitted payload doesn't match the channel's type |
| `PluginBusy(String)` | Unload attempted while target/dependent is mid-build |
| `PluginPanicked(String, String)` | `build()` panicked (message included); state rolled back |
| `SelfDependency(String)` | Plugin provides a key it also injects |
| `SelfEventConflict { .. }` | One meta declares a channel with two different payload types |
| `EventDeclConflict { .. }` | Two plugins declare the same channel with different types |
| `EventChannelMismatch { .. }` | Declaration disagrees with an existing registered channel |

---

# Concurrency contract

- `Context` is `Clone` + `Send` + `Sync`; every method works from any thread or task.
- Internal state uses std `RwLock`/`Mutex` with a fixed acquisition order:
  **graph → services → events**. Critical sections are short and contain **no await
  points**, so nothing blocks the async runtime.
- Emit releases the registry lock before invoking handlers (snapshot pattern), so
  re-registering from inside a handler is safe.
- Concurrent loads from multiple threads are safe and tested:
  racing same-name or conflicting-key loads resolve to a single winner; the rest get the
  corresponding error.
- `unload` vs in-progress `build` is arbitrated via the *building* set (`PluginBusy`).
- The framework itself spawns **no threads**; parallelism comes from your tokio runtime
  (`rt-multi-thread`) driving detached handlers, and from OS threads you own calling into
  `Context`.

Verified by integration tests (`core/tests/context.rs`): 16-thread concurrent
registrations, 8-way racing loads, conflicting-provide races, unload-during-build,
sync-only operation without a runtime, and multi-listener fan-out.

---

# Guidance for plugin authors

**Do:**

- Keep `build()` fast and side-effect-light: provide, inject, register listeners.
- Prefer async listeners (`on_key` / `Events::on`) for anything I/O-bound.
- Use `emit_detached` only when you truly want fire-and-forget; prefer collecting and
  awaiting/joining futures when completion matters.
- Declare `emits`/`listens`/`waterfalls` honestly in `meta()` — it buys you load-time conflict
  detection and accurate introspection.

**Avoid (current sharp edges):**

- **Emitting during `build()`** — plugins loaded later haven't registered listeners yet,
  and a sync listener could re-enter `load`/`unload` mid-activation. Emit after the
  system signals readiness instead.
- **Spawning background tasks in `build()`** that hold a cloned `Context`: if `build()`
  fails afterwards, the atomic rollback removes services/listeners but **cannot** cancel
  your already-spawned task. Spawn from a listener, or make the task tolerate missing
  services.
- **Long-running work inline in sync listeners** — they block the emitting thread and all
  subsequent listeners on that emit.
- Assuming detached handlers die with their plugin: unloading removes *future* deliveries
  but not handlers already spawned.
