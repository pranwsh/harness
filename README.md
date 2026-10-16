# harness

Single-binary agent harness TUI (Rust workspace). All plugins are rlibs compiled into one `harness` binary. See [Linkage](#linkage) for what "single-binary" does and does not mean.

## Install (Nix, declarative)

Requires Nix with flakes. Supports `x86_64-linux`, `aarch64-linux`, `aarch64-darwin`.

Add as a flake input:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    harness.url = "github:pranwsh/harness";
  };
}
```

NixOS (`configuration.nix` / flake module):

```nix
{ inputs, pkgs, ... }:
{
  environment.systemPackages = [
    inputs.harness.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
```

Home Manager (standalone `home.nix` or NixOS `home-manager` module):

```nix
{ inputs, pkgs, ... }:
{
  home.packages = [
    inputs.harness.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
```

Then rebuild (`sudo nixos-rebuild switch --flake .` / `home-manager switch --flake .`). No `nix run` / `nix build` / `nix develop` needed; `nix flake check` (build + clippy `--deny warnings` + tests) is for hacking on `harness` itself.

### Config from Home Manager

`programs.harness` generates the config file for you. Import the module:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    harness.url = "github:pranwsh/harness";
  };
}
```

```nix
# home.nix
{ inputs, pkgs, ... }:
{
  imports = [ inputs.harness.homeManagerModules.default ];

  programs.harness = {
    enable = true;
    settings = {
      llm = {
        base_url = "https://opencode.ai/zen/v1";
        model = "muse-spark-1.3-contributor-free";
        api_key = "public";
        user_agent = "opencode/1.18.18";
        headers = {
          HTTP-Referer = "https://opencode.ai/";
          X-Title = "opencode";
        };
      };
      agent.max_iterations = 128;
      # Optional; omit a key (or set it to `null`) and the built-in default applies.
      session.enabled = true;
      skills.enabled = true;
      mcp.servers.fetch = { command = "npx"; args = [ "-y" "@modelcontextprotocol/server-fetch" ]; };
    };
  };
}
```

`settings` mirrors the sections in `config.example.toml`. Only `llm.base_url`, `llm.model` and `llm.api_key` are required — a missing `[llm]` is a parse error and `harness` exits at startup.

Two caveats:

- **The generated file is read-only.** It is a symlink into the Nix store, and the model popup persists a choice by rewriting `[llm] model` in place. That write fails against a store path; the choice still applies for the session and the notice reads `config save failed`. Change `settings.llm.model` and re-run `home-manager switch` to persist it.
- **`harness` is wrapped.** The app resolves config as `./config.toml` in the *current directory*, with no XDG fallback (`plugins/config/src/lib.rs`), so it cannot find a file managed at `$XDG_CONFIG_HOME/harness/config.toml` on its own. The module therefore installs a wrapper that passes `--config` for you. Unset `settings` and you get the plain binary plus no managed file.

With `settings` unset the module changes nothing beyond installing the package, which is the same as the `home.packages` snippet above.

## Development

The dev environment is project-local; nothing needs to be installed
system-wide. If you use [direnv](https://direnv.net), the included `.envrc`
loads the flake's dev shell automatically on `cd`:

```sh
direnv allow   # once, to trust .envrc
cargo build    # works directly, no global gcc needed
```

Without direnv, enter the same environment manually:

```sh
nix develop
# or, for a single command:
nix develop --command cargo build
```

The dev shell provides `rustc`, `cargo`, `clippy`, `rustfmt` and
`stdenv.cc`. The C compiler matters: `ring` (pulled in by `rustls`, which
`reqwest` enables via its `rustls-tls` feature) builds C and assembly through
the `cc` crate, and rustc shells out to `cc` to link every crate's build
script. On a machine with no system compiler, a bare `cargo build` fails with
`error: linker `cc` not found` — the dev shell supplies `cc` for you.

The toolchain version comes from the pinned `nixpkgs` input in
`flake.lock`, so there is no separate toolchain pin to keep in sync.

Checks: `nix flake check` (build + clippy `--deny warnings` + tests).

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
  -> plugins/* (rlibs, linked into the binary)
```

## Linkage

`harness` is a single self-contained binary in the sense that matters here: all
25 plugins are compiled in as Rust rlibs and there is nothing else to install.
It is not a fully static executable — the C runtime is linked dynamically
against the platform libc (glibc on Linux, system `libSystem` on Darwin), plus
`libgcc_s.so.1`. Those come from the host system, so the Nix derivation is the
right distribution mechanism. Fully static linking is possible with crane via
`muslOverride` / `pkgsStatic`, but is not configured here.

- **core (`harness-core`):** plugin runtime. Plugins declare `meta()` (`provides`/`injects` services by string `Key`, `emits`/`listens`/`waterfalls` typed event channels) plus a one-shot `build(ctx)` hook. Missing deps park as pending and auto-activate; unload cascades to dependents.
- **contracts (`harness-contracts`):** shared vocabulary only (keys, domain/event types, `*_API` trait handles). Domain plugins depend on `contracts` + `core`, never on each other; sibling `Plugin` deps live only in `[dev-dependencies]` for wiring tests. All runtime cross-plugin comms go through trait handles under `KEY_*_API` / store keys or the core bus — the crate graph stays acyclic. Two intentional direct edges are documented in `contracts/src/lib.rs`: the TUI leaf shell (`tui` → `agent-loop` / `tui-*`) and the shared hashing infra (`hashline-*` → `hash-base`).
- **harness:** thin `load!` list (config → model → agent → tools → shell → agent-loop → tui-*). Order is convenience — core reorders/parks as needed.
- **plugins:**
  - infra: `config` (toml loading), `session` (conversation log, journaled to `$XDG_DATA_HOME/harness/sessions` — see `[session]` in `config.example.toml`), `skills` (Agent Skills registry over `$XDG_CONFIG_HOME/harness/skills` — see `[skills]` in `config.example.toml`)
  - llm/agent: `model` (HTTP transport), `model-headers` (header waterfall), `agent`, `agent-default-model`, `agent-loop`, `system-prompt`
  - tools: `tools` (registry/executor), `shell` (`shell_exec/start/poll/stop`), `mcp` (MCP stdio bridge: `mcp__<server>__<tool>`), `hash-base`, `hashline-read`, `hashline-edit` (incl. rev-less `set_n`/`insert_n`/`delete_n`/`replace_n`), `file-write`
  - tui: `tui` (runner), `tui-state`, `tui-input`, `tui-markdown`, `tui-popup`, `tui-filter` (shared filter-popup behavior), `tui-commands` (slash completion), `tui-model` (model search), `tui-sessions` (`/sessions` picker: list past sessions, resume on select), `tui-branch` (`/branch` navigator: rewind the current session to any turn, branch from there)

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

### Secrets (`llmApiKeyFile`)

Anything in `settings` lands in the Nix store (world-readable), so don't
put `llm.api_key` there. Set `programs.harness.llmApiKeyFile` to a runtime
path (e.g. a sops-nix secret under `/run/secrets`) instead:

```nix
programs.harness = {
  enable = true;
  llmApiKeyFile = "/run/secrets/ninerouter_key";
  settings.llm = {
    base_url = "http://localhost:20128/v1";
    model = "oc/muse-spark-1.3-contributor-free";
    # No api_key here: the managed file carries a placeholder and the
    # wrapper substitutes the real key into a `0600` copy under
    # `$XDG_RUNTIME_DIR` on each launch.
  };
};
```

Any `settings.llm.api_key` is replaced by the placeholder before rendering,
so it never reaches the store. `llmApiKeyFile` requires `settings` (there
has to be a generated file to substitute into).
