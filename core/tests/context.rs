use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
};

use harness_core::{Context, Error, Events, Key, LoadOutcome, Plugin, PluginMeta};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
struct Num(u32);

struct Provider {
    meta: PluginMeta,
    key: Key,
    value: u32,
}

impl Provider {
    fn new(name: &str, key: &str, value: u32) -> Self {
        Self {
            meta: PluginMeta::new(name).provides(key),
            key: Key::new(key),
            value,
        }
    }
}

impl Plugin for Provider {
    fn meta(&self) -> PluginMeta {
        self.meta.clone()
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key(self.key.clone(), Arc::new(Num(self.value)));
        Ok(())
    }
}

struct Consumer {
    meta: PluginMeta,
    keys: Vec<Key>,
}

impl Consumer {
    fn new(name: &str, keys: &[&str]) -> Self {
        let mut meta = PluginMeta::new(name);
        for k in keys {
            meta = meta.injects(*k);
        }
        Self {
            meta,
            keys: keys.iter().map(|k| Key::new(*k)).collect(),
        }
    }
}

impl Plugin for Consumer {
    fn meta(&self) -> PluginMeta {
        self.meta.clone()
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        for key in &self.keys {
            ctx.inject_key::<Num>(key.clone())?;
        }
        Ok(())
    }
}

struct Bridge {
    meta: PluginMeta,
    needs: Key,
    gives: Key,
}

impl Bridge {
    fn new(name: &str, needs: &str, gives: &str) -> Self {
        Self {
            meta: PluginMeta::new(name).injects(needs).provides(gives),
            needs: Key::new(needs),
            gives: Key::new(gives),
        }
    }
}

impl Plugin for Bridge {
    fn meta(&self) -> PluginMeta {
        self.meta.clone()
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let upstream = ctx.inject_key::<Num>(self.needs.clone())?;
        ctx.provide_key(self.gives.clone(), Arc::new(Num(upstream.0 + 1)));
        Ok(())
    }
}

struct SelfDependent;

impl Plugin for SelfDependent {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("selfdep").provides("x").injects("x")
    }

    fn build(&self, _ctx: Context) -> harness_core::Result<()> {
        Ok(())
    }
}

struct ChildPlugin;

impl Plugin for ChildPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("child").provides("child.svc")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key("child.svc", Arc::new(Num(1)));
        Ok(())
    }
}

struct EmitterPlugin;

impl Plugin for EmitterPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("emitter").provides("evt.svc")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key("evt.svc", Arc::new(Num(1)));
        ctx.on_sync_key::<Num, _>("evt.chan", |_| {}).unwrap();
        Ok(())
    }
}

struct PanickyPlugin;

impl Plugin for PanickyPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("panicky").provides("p.svc")
    }

    fn build(&self, _ctx: Context) -> harness_core::Result<()> {
        panic!("boom");
    }
}

struct GatedPlugin {
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl Plugin for GatedPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("gated").provides("gated.svc")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        self.started.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        ctx.provide_key("gated.svc", Arc::new(Num(1)));
        Ok(())
    }
}

struct ParentPlugin;

impl Plugin for ParentPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("parent").provides("parent.svc")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key("parent.svc", Arc::new(Num(2)));
        assert!(matches!(
            ctx.load(ChildPlugin).unwrap(),
            LoadOutcome::Activated
        ));
        Ok(())
    }
}

struct ChanListener {
    name: String,
    chan: Key,
    log: Option<Arc<Mutex<Vec<String>>>>,
}

impl Plugin for ChanListener {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(self.name.clone())
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let chan = self.chan.clone();
        ctx.on_sync_key::<Num, _>(chan, |_| {}).unwrap();
        if let Some(log) = &self.log {
            log.lock().unwrap().push(self.name.clone());
        }
        Ok(())
    }
}

struct ChanEmitter {
    name: String,
    listen: Option<Key>,
    log: Option<Arc<Mutex<Vec<String>>>>,
}

impl ChanEmitter {
    fn emitter(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            listen: None,
            log: None,
        }
    }
}

impl Plugin for ChanEmitter {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(self.name.clone())
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        if let Some(key) = &self.listen {
            let key = key.clone();
            ctx.on_sync_key::<Num, _>(key, |_| {}).unwrap();
        }
        if let Some(log) = &self.log {
            log.lock().unwrap().push(self.name.clone());
        }
        Ok(())
    }
}

#[test]
fn key_derivation() {
    assert_eq!(Key::new("db.pool"), Key::from("db.pool"));
    assert_ne!(Key::new("a"), Key::new("b"));
}

#[tokio::test]
async fn key_inject_roundtrip() {
    let ctx = Context::root();
    assert!(ctx.try_inject_key::<Num>("num").is_none());
    ctx.provide_key("num", Arc::new(Num(7)));
    assert_eq!(ctx.inject_key::<Num>("num").unwrap().0, 7);
    assert!(matches!(
        ctx.inject_key::<String>("str"),
        Err(Error::MissingService(k)) if k == Key::new("str")
    ));
}

#[tokio::test]
async fn explicit_key_inject() {
    let ctx = Context::root();
    ctx.provide_key("db.pool", Arc::new(Num(1)));
    assert_eq!(ctx.inject_key::<Num>("db.pool").unwrap().0, 1);
    assert!(ctx.inject_key::<String>("db.pool").is_err());
}

#[tokio::test]
async fn defer_then_activate() {
    let ctx = Context::root();

    let outcome = ctx.load(Consumer::new("consumer", &["a.svc"])).unwrap();
    assert_eq!(
        outcome,
        LoadOutcome::Pending {
            missing: vec![Key::new("a.svc")]
        }
    );
    assert_eq!(ctx.pending_names(), vec!["consumer"]);
    assert!(ctx.plugin_names().is_empty());

    ctx.load(Provider::new("provider-a", "a.svc", 5)).unwrap();
    assert!(ctx.plugin_names().contains(&"consumer".to_owned()));
    assert!(ctx.pending_names().is_empty());
    assert_eq!(
        ctx.provider_of(&Key::new("a.svc")).as_deref(),
        Some("provider-a")
    );
}

#[tokio::test]
async fn multi_dep_gate() {
    let ctx = Context::root();
    ctx.load(Consumer::new("greedy", &["k.one", "k.two"]))
        .unwrap();

    ctx.load(Provider::new("p1", "k.one", 1)).unwrap();
    assert_eq!(ctx.pending_names(), vec!["greedy"]);

    ctx.load(Provider::new("p2", "k.two", 2)).unwrap();
    assert!(ctx.pending_names().is_empty());
    assert!(ctx.plugin_names().contains(&"greedy".to_owned()));
}

#[tokio::test]
async fn cascade_unload_order() {
    let ctx = Context::root();
    ctx.load(Provider::new("a", "a.svc", 1)).unwrap();
    ctx.load(Bridge::new("b", "a.svc", "b.svc")).unwrap();
    ctx.load(Consumer::new("c", &["b.svc"])).unwrap();
    assert_eq!(ctx.plugin_names().len(), 3);

    let cascaded = ctx.unload("a").unwrap();
    assert_eq!(cascaded, vec!["c".to_owned(), "b".to_owned()]);
    assert!(ctx.plugin_names().is_empty());
}

#[tokio::test]
async fn unload_pending_dequeues() {
    let ctx = Context::root();
    ctx.load(Consumer::new("waiting", &["nope.svc"])).unwrap();
    assert_eq!(ctx.unload("waiting").unwrap(), Vec::<String>::new());
    assert!(ctx.pending_names().is_empty());
    assert!(matches!(
        ctx.unload("waiting"),
        Err(Error::UnknownPlugin(_))
    ));
}

#[tokio::test]
async fn duplicate_plugin_rejected() {
    let ctx = Context::root();
    ctx.load(Provider::new("dup", "d.svc", 1)).unwrap();
    assert!(matches!(
        ctx.load(Provider::new("dup", "other.svc", 2)),
        Err(Error::DuplicatePlugin(_))
    ));

    ctx.load(Consumer::new("waiter", &["x.svc"])).unwrap();
    assert!(matches!(
        ctx.load(Consumer::new("waiter", &[])),
        Err(Error::DuplicatePlugin(_))
    ));
}

#[tokio::test]
async fn service_conflict_between_plugins() {
    let ctx = Context::root();
    ctx.load(Provider::new("first", "shared.svc", 1)).unwrap();

    match ctx
        .load(Provider::new("second", "shared.svc", 2))
        .unwrap_err()
    {
        Error::ServiceConflict { key, provider } => {
            assert_eq!(key, Key::new("shared.svc"));
            assert_eq!(provider, "first");
        }
        other => panic!("unexpected: {other}"),
    }
    assert!(!ctx.plugin_names().contains(&"second".to_owned()));
}

#[tokio::test]
async fn service_conflict_with_system() {
    let ctx = Context::root();
    ctx.provide_key("sys.svc", Arc::new(Num(0)));
    assert!(matches!(
        ctx.load(Provider::new("squatter", "sys.svc", 1)),
        Err(Error::ServiceConflict { .. })
    ));
}

#[test]
fn self_dependency_rejected() {
    let ctx = Context::root();
    assert!(matches!(
        ctx.load(SelfDependent),
        Err(Error::SelfDependency(_))
    ));
}

#[tokio::test]
async fn nested_load_is_independent() {
    let ctx = Context::root();
    ctx.load(ParentPlugin).unwrap();
    assert!(ctx.plugin_names().contains(&"child".to_owned()));

    ctx.unload("parent").unwrap();
    assert!(ctx.plugin_names().contains(&"child".to_owned()));
    assert!(!ctx.plugin_names().contains(&"parent".to_owned()));
}

#[tokio::test]
async fn keyed_emit_roundtrip() {
    let ctx = Context::root();
    let (tx, mut rx) = mpsc::unbounded_channel();
    ctx.on_key::<Num, _, _>("num.tick", move |n| {
        let tx = tx.clone();
        async move {
            tx.send(n.0).unwrap();
        }
    })
    .unwrap();

    for fut in ctx.emit_key("num.tick", Num(3)).unwrap() {
        fut.await;
    }
    assert_eq!(rx.recv().await, Some(3));
}

#[tokio::test]
async fn on_sync_key_receives() {
    let ctx = Context::root();
    let (tx, mut rx) = mpsc::unbounded_channel();
    ctx.on_sync_key::<Num, _>("sync.chan", move |n| {
        tx.send(n.0).unwrap();
    })
    .unwrap();

    ctx.emit_key("sync.chan", Num(9)).unwrap();
    assert_eq!(rx.recv().await, Some(9));
}

#[tokio::test]
async fn multiple_listeners_all_fire() {
    let ctx = Context::root();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for _ in 0..3 {
        let tx = tx.clone();
        ctx.on_key::<Num, _, _>("fanout", move |n| {
            let tx = tx.clone();
            async move {
                tx.send(n.0).unwrap();
            }
        })
        .unwrap();
    }

    for fut in ctx.emit_key("fanout", Num(1)).unwrap() {
        fut.await;
    }
    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(rx.recv().await.unwrap());
    }
    assert_eq!(got, vec![1, 1, 1]);
}

#[tokio::test]
async fn same_type_rejoin_allowed() {
    let ctx = Context::root();
    ctx.on_key::<Num, _, _>("chan", |_| async {}).unwrap();
    ctx.on_key::<Num, _, _>("chan", |_| async {}).unwrap();
}

#[tokio::test]
async fn channel_type_conflict_rejected() {
    let ctx = Context::root();
    ctx.on_key::<Num, _, _>("chan", |_| async {}).unwrap();
    assert!(matches!(
        ctx.on_key::<String, _, _>("chan", |_| async {}),
        Err(Error::EventConflict { key }) if key == Key::new("chan")
    ));
}

#[tokio::test]
async fn emit_unknown_channel_is_noop() {
    let ctx = Context::root();
    let fired = ctx.emit_key("ghost", Num(1)).unwrap();
    assert!(fired.is_empty());
}

#[tokio::test]
async fn emit_wrong_type_errors() {
    let ctx = Context::root();
    ctx.load(EmitterPlugin).unwrap();
    assert!(matches!(
        ctx.emit_key("evt.chan", String::from("wrong")),
        Err(Error::PayloadTypeMismatch { key }) if key == Key::new("evt.chan")
    ));
}

#[tokio::test]
async fn unload_purges_listeners_and_services() {
    let ctx = Context::root();
    ctx.load(EmitterPlugin).unwrap();
    ctx.emit_key("evt.chan", Num(1)).unwrap();

    ctx.unload("emitter").unwrap();
    assert!(ctx.emit_key("evt.chan", Num(1)).unwrap().is_empty());
    assert!(matches!(
        ctx.load(Provider::new("again", "evt.svc", 2)),
        Ok(LoadOutcome::Activated)
    ));
}

#[test]
fn concurrent_registrations_do_not_lose_writes() {
    let ctx = Context::root();
    let handles: Vec<_> = (0..16u32)
        .map(|i| {
            let ctx = ctx.clone();
            thread::spawn(move || {
                ctx.provide_key(format!("k{i}"), Arc::new(Num(i)));
                ctx.on_sync_key::<Num, _>(format!("c{i}"), |_| {}).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    for i in 0..16u32 {
        assert_eq!(ctx.try_inject_key::<Num>(format!("k{i}")).unwrap().0, i);
        ctx.emit_key(format!("c{i}"), Num(1)).unwrap();
    }
}

#[test]
fn sync_emit_without_runtime() {
    let ctx = Context::root();
    let hits = Arc::new(AtomicU32::new(0));
    let counter = hits.clone();
    ctx.on_sync_key::<Num, _>("bare.chan", move |n| {
        counter.fetch_add(n.0, Ordering::Relaxed);
    })
    .unwrap();

    let pending = ctx.emit_key("bare.chan", Num(5)).unwrap();
    assert!(pending.is_empty());
    assert_eq!(hits.load(Ordering::Relaxed), 5);
}

#[test]
fn concurrent_same_name_loads_single_winner() {
    for _ in 0..20 {
        let ctx = Context::root();
        let handles: Vec<_> = (0..8u32)
            .map(|i| {
                let ctx = ctx.clone();
                thread::spawn(move || ctx.load(Provider::new("dupe", "d.svc", i)))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "exactly one winner per round"
        );
        assert_eq!(ctx.provider_of(&Key::new("d.svc")).as_deref(), Some("dupe"));
        assert_eq!(ctx.plugin_names(), vec!["dupe"]);
    }
}

#[test]
fn concurrent_conflicting_provides_single_winner() {
    for _ in 0..20 {
        let ctx = Context::root();
        let handles: Vec<_> = (0..6u32)
            .map(|i| {
                let ctx = ctx.clone();
                let name = format!("p{i}");
                thread::spawn(move || ctx.load(Provider::new(&name, "shared.svc", i)))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        for r in results {
            match r {
                Ok(_) | Err(Error::ServiceConflict { .. }) => {}
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert_eq!(ctx.plugin_names().len(), 1);
    }
}

#[test]
fn unload_during_build_reports_busy() {
    let ctx = Context::root();
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let plugin = GatedPlugin {
        started: started.clone(),
        release: release.clone(),
    };

    let loader_ctx = ctx.clone();
    let loader = thread::spawn(move || loader_ctx.load(plugin));

    while !started.load(Ordering::SeqCst) {
        thread::yield_now();
    }

    assert!(matches!(
        ctx.unload("gated"),
        Err(Error::PluginBusy(n)) if n == "gated"
    ));

    release.store(true, Ordering::SeqCst);
    assert!(matches!(loader.join().unwrap(), Ok(LoadOutcome::Activated)));
    assert!(ctx.plugin_names().contains(&"gated".to_owned()));
    ctx.unload("gated").unwrap();
    assert!(!ctx.plugin_names().contains(&"gated".to_owned()));
}

#[test]
fn panicked_build_returns_error_and_rolls_back() {
    let ctx = Context::root();

    let err = ctx.load(PanickyPlugin).unwrap_err();
    assert!(matches!(
        err,
        Error::PluginPanicked(name, msg) if name == "panicky" && msg == "boom"
    ));

    assert!(!ctx.plugin_names().contains(&"panicky".to_owned()));
    assert_eq!(ctx.provider_of(&Key::new("p.svc")), None);

    struct Replacement;

    impl Plugin for Replacement {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("panicky").provides("p.svc")
        }

        fn build(&self, ctx: Context) -> harness_core::Result<()> {
            ctx.provide_key("p.svc", Arc::new(Num(9)));
            Ok(())
        }
    }

    assert!(matches!(ctx.load(Replacement), Ok(LoadOutcome::Activated)));
    assert_eq!(ctx.inject_key::<Num>("p.svc").unwrap().0, 9);
}

#[tokio::test]
async fn emitter_activates_without_listeners() {
    let ctx = Context::root();

    let outcome = ctx.load(ChanEmitter::emitter("emitter")).unwrap();
    assert_eq!(outcome, LoadOutcome::Activated);
    assert!(ctx.pending_names().is_empty());
    assert_eq!(ctx.plugin_names(), vec!["emitter"]);
    assert!(ctx.emit_key("never.registered", Num(1)).unwrap().is_empty());
}

#[tokio::test]
async fn unload_last_listener_keeps_emitter_active() {
    let ctx = Context::root();
    ctx.load(ChanListener {
        name: "listener".to_owned(),
        chan: Key::new("c.chan"),
        log: None,
    })
    .unwrap();
    ctx.load(ChanEmitter::emitter("emitter")).unwrap();
    assert_eq!(ctx.plugin_names().len(), 2);

    let cascaded = ctx.unload("listener").unwrap();
    assert!(cascaded.is_empty());
    assert!(ctx.plugin_names().contains(&"emitter".to_owned()));
    assert!(ctx.emit_key("c.chan", Num(1)).unwrap().is_empty());
}

#[tokio::test]
async fn emitter_survives_with_remaining_listener() {
    let ctx = Context::root();
    for name in ["l1", "l2"] {
        ctx.load(ChanListener {
            name: name.to_owned(),
            chan: Key::new("d.chan"),
            log: None,
        })
        .unwrap();
    }
    ctx.load(ChanEmitter::emitter("emitter")).unwrap();

    let cascaded = ctx.unload("l1").unwrap();
    assert!(cascaded.is_empty());
    assert!(ctx.plugin_names().contains(&"emitter".to_owned()));
    ctx.emit_key("d.chan", Num(1)).unwrap();
}

#[tokio::test]
async fn events_handle_injectable_roundtrip() {
    struct BusOwner;
    struct Subscriber {
        seen: Arc<Mutex<Vec<u32>>>,
    }

    impl Plugin for BusOwner {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("bus-owner").provides("bus.handle")
        }

        fn build(&self, ctx: Context) -> harness_core::Result<()> {
            let bus = ctx.events::<Num>("h.chan");
            ctx.provide_key("bus.handle", Arc::new(bus));
            Ok(())
        }
    }

    impl Plugin for Subscriber {
        fn meta(&self) -> PluginMeta {
            PluginMeta::new("subscriber").injects("bus.handle")
        }

        fn build(&self, ctx: Context) -> harness_core::Result<()> {
            let bus: Arc<Events<Num>> = ctx.inject_key("bus.handle")?;
            let seen = self.seen.clone();
            bus.on(move |n| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(n.0);
                }
            })?;
            Ok(())
        }
    }

    let ctx = Context::root();
    let seen = Arc::new(Mutex::new(Vec::new()));
    ctx.load(BusOwner).unwrap();
    ctx.load(Subscriber { seen: seen.clone() }).unwrap();

    let bus = ctx.events::<Num>("h.chan");
    for fut in bus.emit(Num(42)).unwrap() {
        fut.await;
    }
    for fut in bus.emit(Num(7)).unwrap() {
        fut.await;
    }
    assert_eq!(seen.lock().unwrap().clone(), vec![42, 7]);
}
