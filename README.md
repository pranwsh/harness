# harness

Single-binary agent harness TUI (Rust workspace, statically linked). All plugins are rlibs compiled into one `harness` binary.

## Install (Nix)

Requires Nix with flakes. Supports `x86_64-linux`, `aarch64-linux`, `aarch64-darwin`.

```sh
# run directly
nix run

# build binary -> ./result/bin/harness
nix build

# dev shell (rustc, cargo, clippy, rustfmt)
nix develop

# checks: build + clippy (`--deny warnings`) + tests
nix flake check
```

Configure:

```sh
cp config.example.toml ./config.toml
# then edit ./config.toml
```

Config resolution: `./config.toml` (cwd-relative) by default, or pass an
explicit path:

```sh
harness --config PATH
```

`--config` takes a separate value only (`--config PATH`, exact match;
`--config=PATH` is rejected). Any other flag or positional argument is an
error. Packaged example also at `$out/share/doc/harness/config.example.toml`.

## Architecture

```
harness/src/main.rs (composition root)
  -> core (Context: services + event bus + dependency graph)
  -> contracts (shared keys/types)
  -> plugins/* (rlibs, statically linked)
```

- **core (`harness-core`):** plugin runtime. Plugins declare `meta()` (`provides`/`injects` services by string `Key`, `emits`/`listens`/`waterfalls` typed event channels) plus a one-shot `build(ctx)` hook. Missing deps park as pending and auto-activate; unload cascades to dependents.
- **contracts (`harness-contracts`):** shared vocabulary only (keys, domain/event types, `*_API` trait handles). Domain plugins depend on `contracts` + `core`, never on each other; sibling `Plugin` deps live only in `[dev-dependencies]` for wiring tests. All runtime cross-plugin comms go through trait handles under `KEY_*_API` / store keys or the core bus — the crate graph stays acyclic. Two intentional direct edges are documented in `contracts/src/lib.rs`: the TUI leaf shell (`tui` → `agent-loop` / `tui-*`) and the shared hashing infra (`hashline-*` → `hash-base`).
- **harness:** thin `load!` list (config → model → agent → tools → shell → agent-loop → tui-*). Order is convenience — core reorders/parks as needed.
- **plugins:**
  - infra: `config` (toml loading), `session` (conversation log, journaled to `$XDG_DATA_HOME/harness/sessions` — see `[session]` in `config.example.toml`)
  - llm/agent: `model` (HTTP transport), `model-headers` (header waterfall), `agent`, `agent-default-model`, `agent-loop`, `system-prompt`
  - tools: `tools` (registry/executor), `shell` (`shell_exec/start/poll/stop`), `mcp` (MCP stdio bridge: `mcp__<server>__<tool>`), `hash-base`, `hashline-read`, `hashline-edit`
  - tui: `tui` (runner), `tui-state`, `tui-input`, `tui-markdown`, `tui-popup`, `tui-filter` (shared filter-popup behavior), `tui-commands` (slash completion), `tui-model` (model search), `tui-sessions` (`/sessions` picker: list past sessions, resume on select)

## Shell + sudo note

`shell_exec` / `shell_start` run headless: stdin is null, stdout/stderr are
piped, and every child is `setsid`-detached so it has no controlling tty
(see `detach_tty` in `plugins/shell/src/env.rs`).

Why: piping alone doesn't stop `sudo` — it re-opens `/dev/tty` directly for
the `Password:` prompt, which paints over the TUI input box and steals
keystrokes. With no ctty that open fails, so `sudo` exits fast with
`sudo: no tty present` in captured stderr instead of corrupting the screen.

Consequence: interactive `sudo` (password entry) is unsupported by design.
Use `sudo -n <cmd>` (fails fast without a prompt) or pre-auth the tty
outside the harness. A password-popup flow (`tui-password` + `sudo -S` over
a pipe) is possible but intentionally deferred — see issue notes.
