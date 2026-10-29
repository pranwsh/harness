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

The Home Manager module exposes every option in `config.example.toml` through
`programs.harness.settings`: `llm`, `agent`, `session`, `skills`, `shell`, and
`mcp.servers.<name>`. The following is a complete practical configuration,
based on a local NixOS setup. Adjust the endpoint, model, secret path, and MCP
server environment for your machine:

```nix
# home/dev/harness.nix
{ inputs, ... }:
{
  imports = [ inputs.harness.homeManagerModules.default ];

  programs.harness = {
    enable = true;

    # Keep API keys out of settings: Nix settings are stored in the world-readable
    # Nix store. This file should be provisioned by your secret manager.
    llmApiKeyFile = "/run/secrets/ninerouter_key";

    settings = {
      llm = {
        base_url = "http://localhost:20128/v1";
        model = "oc/muse-spark-1.3-contributor-free";
        # Optional LLM options, if needed:
        # user_agent = "my-client/1.0";
        # headers = { HTTP-Referer = "https://example.org"; X-Title = "my harness"; };
        # responses_models = [ "some-responses-only-model" ];
      };

      agent.max_iterations = 128;

      # Optional session persistence; enabled by default. `dir` can override
      # the XDG data directory.
      session = {
        enabled = true;
        # dir = "/home/alice/.local/share/harness/sessions";
      };

      # Optional Agent Skills catalog; enabled by default. `dir` can override
      # the XDG config directory containing skill subdirectories.
      skills = {
        enabled = true;
        # dir = "/home/alice/.config/harness/skills";
      };

      # Shell resource limits (these are limits, not command allowlists).
      shell = {
        default_timeout_ms = 30000;
        max_timeout_ms = 120000;
        max_output_bytes = 65536;
        max_capture_bytes = 1048576;
        max_jobs = 32;
        max_job_time_ms = 600000;
        max_job_output_bytes = 262144;
      };

      # Each named MCP server is started as a persistent stdio child process.
      # The example starts mcp-searxng through Nix's Node package; configure
      # server-specific variables for your local SearXNG instance.
      mcp.servers.searxng = {
        command = "nix";
        args = [
          "shell"
          "nixpkgs#nodejs_22"
          "--command"
          "npx"
          "-y"
          "mcp-searxng"
        ];
        env = {
          SEARXNG_URL = "http://127.0.0.1:8888";
          SEARXNG_FANOUT = "false";
          SEARXNG_TIMEOUT_MS = "10000";
          SEARXNG_MAX_RESULTS = "5";
          SEARXNG_HTML_FALLBACK = "false";
          NO_PROXY = "127.0.0.1,localhost";
        };
        # Optional per-tool-call timeout and disable switch:
        # timeout_ms = 30000;
        # disabled = false;
      };
    };
  };
}
```

Import `inputs.harness.homeManagerModules.default` in your Home Manager module
and provide `inputs` to it. For example, in a flake-based Home Manager setup:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    harness.url = "github:pranwsh/harness";
  };
}
```

The required LLM settings are `llm.base_url` and `llm.model`. Normally set
`llmApiKeyFile` and leave `llm.api_key` out of `settings`: values in `settings`
are rendered into the Nix store. If you intentionally use a non-secret key,
you may set `llm.api_key` in `settings` instead. The optional LLM fields are
`user_agent`, `headers`, and `responses_models`; see `config.example.toml` for
details.

All fields have defaults where documented in `config.example.toml`; omit
optional fields to use those defaults. Session persistence and Skills are on by
default. MCP server options are `command` (required), `args`, `env`, optional
`timeout_ms`, and `disabled`.

Two implementation details:

- **The managed config is writable at runtime.** Nix generates a read-only
template, while the wrapper materializes a writable live file so the `/model`
picker can persist model selections. The chosen model survives restarts and
Home Manager switches; other settings follow the Nix declaration. With
`llmApiKeyFile`, the app uses a `0600` runtime copy under `$XDG_RUNTIME_DIR` so
the key is not written to durable config.
- **The wrapper passes `--config` explicitly.** This honours custom
`xdg.configHome` and `programs.harness.configFileName`. Without `settings`, the
module just installs the binary and does not generate or manage a config file.

After adding or changing settings, rebuild with `home-manager switch --flake .`
or `sudo nixos-rebuild switch --flake .` (for NixOS-integrated Home Manager).

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

mkdir -p ~/.config/harness
cp config.example.toml ~/.config/harness/config.toml
# then edit ~/.config/harness/config.toml
```

Config resolution: `$XDG_CONFIG_HOME/harness/config.toml`, else
`$HOME/.config/harness/config.toml`, else `./config.toml` (cwd-relative),
or pass an explicit path:

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
  - tools: `tools` (registry/executor), `shell` (`shell_exec/start/poll/stop`), `mcp` (MCP stdio bridge: `mcp__<server>__<tool>`), `hash-base`, `hashline-read` (hashed rows by default, `grep` with `ignore_case`/`context`, directory listings), `hashline-edit` (hash and line ops, `sub`, `dry_run`, one-step `revert`), `file-write`
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
