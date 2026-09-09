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
cp config.example.toml config.toml
# or: HARNESS_CONFIG=./config.toml harness
```

Resolution: `$HARNESS_CONFIG`, else `./config.toml` (cwd-relative). Packaged example also at `$out/share/doc/harness/config.example.toml`.

## Architecture

```
harness/src/main.rs (composition root)
  -> core (Context: services + event bus + dependency graph)
  -> contracts (shared keys/types)
  -> plugins/* (rlibs, statically linked)
```

- **core (`harness-core`):** plugin runtime. Plugins declare `meta()` (`provides`/`injects` services by string `Key`, `emits`/`listens`/`waterfalls` typed event channels) plus a one-shot `build(ctx)` hook. Missing deps park as pending and auto-activate; unload cascades to dependents.
- **contracts (`harness-contracts`):** shared vocabulary only (keys, domain/event types). Plugins depend on contracts, never on each other; all cross-plugin comms go through the core bus. Keeps the crate graph acyclic.
- **harness:** thin `load!` list (config → model → agent → tools → shell → agent-loop → tui-*). Order is convenience — core reorders/parks as needed.
- **plugins:**
  - infra: `config` (toml loading), `session` (memory backend)
  - llm/agent: `model` (HTTP transport), `model-headers` (header waterfall), `agent`, `agent-default-model`, `agent-loop`, `system-prompt`
  - tools: `tools` (registry/executor), `shell` (`shell_exec/start/poll/stop`), `hash-base`, `hashline-read`, `hashline-edit`
  - tui: `tui` (runner), `tui-state`, `tui-input`, `tui-markdown`, `tui-popup`, `tui-model`
