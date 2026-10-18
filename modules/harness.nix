# Home Manager module: installs `harness` and generates its config file.
#
#   programs.harness = {
#     enable = true;
#     settings = {
#       llm = {
#         base_url = "https://opencode.ai/zen/v1";
#         model = "muse-spark-1.3-contributor-free";
#         api_key = "public";
#       };
#     };
#   };
#
# `settings` is the config file as an attrset (see `config.example.toml`),
# rendered by `modules/toml.nix`.
#
# The generated file is a read-only *template* in the store, never the file
# `harness` runs against: the app persists a `/model` choice by rewriting
# `[llm] model` in place (`ConfigService::save`) and a store path is
# read-only. The module installs a wrapper that materialises a writable
# config from the template and points `--config` at it, so what Nix
# declares is the source of truth for every setting *except* `model`,
# which the app owns. Concretely:
#
#   * no `llmApiKeyFile` -- the live file is regenerated from the template
#     on each launch (Nix edits take effect on `home-manager switch`) while
#     the previously chosen model is carried over.
#   * with `llmApiKeyFile` -- the key is substituted into a `0600` copy
#     under `$XDG_RUNTIME_DIR` (tmpfs) and the app runs against that; on
#     exit the chosen model is copied to a keyless sidecar at the config
#     path. The key never lands on durable disk.
#
# Secrets: anything in `settings` lands in the Nix store (world-readable),
# so `llm.api_key` must not go there. Set `llmApiKeyFile` to a runtime
# path instead (e.g. `/run/secrets/ninerouter_key`): the template then
# carries a placeholder and the wrapper substitutes the real key at
# launch. Any `settings.llm.api_key` is replaced by the placeholder before
# rendering, so it never reaches the store -- just leave it out.
{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.harness;
  toml = import ./toml.nix {inherit lib;};
  hasSettings = cfg.settings != {};
  useKeyFile = cfg.llmApiKeyFile != null;

  # Rendered as `llm.api_key` when `llmApiKeyFile` is set, so the real key
  # never enters the Nix store. The wrapper replaces this literal string
  # at runtime (plain string substitution, no regex).
  apiKeyPlaceholder = "@@HARNESS_LLM_API_KEY@@";

  effectiveSettings =
    if useKeyFile
    then lib.recursiveUpdate cfg.settings {llm.api_key = apiKeyPlaceholder;}
    else cfg.settings;

  # Absolute path, so an `xdg.configHome` override is honoured instead of
  # writing to a hardcoded `~/.config`. Same directory the session/skills
  # plugins resolve their own dirs from.
  liveConfigPath = "${config.xdg.configHome}/harness/${cfg.configFileName}";

  # The generated config is a *template* in the store: read-only, carrying
  # the key placeholder when `llmApiKeyFile` is set. It is never the file
  # `harness` runs against (see `wrapped`).
  settingsFile =
    if hasSettings
    then pkgs.writeText cfg.configFileName (toml effectiveSettings + "\n")
    else null;

  # Bare `harness` ignores a managed config (see header), so the package on
  # PATH is a wrapper whenever `settings` is set. `--config` is the only
  # flag the CLI accepts, and it takes a separate value, never `--config=PATH`.
  #
  # The app persists `/model` by rewriting the `model` line in the file it
  # was given (`ConfigService::save`), so it must be handed a *writable*
  # file, never the store: with `llmApiKeyFile` the wrapper materialises the
  # key-substituted config under `$XDG_RUNTIME_DIR` (tmpfs, `0600`) -- the
  # key stays off disk exactly as before -- and copies the chosen model back
  # onto the keyless template afterwards; without it the live file is
  # regenerated from the template (carrying the previous model) on every
  # launch. Either way a `home-manager switch` rolls every setting except
  # `model`, which the app owns.
  wrapped =
    if !hasSettings
    then null
    else if useKeyFile
    then
      pkgs.writeShellScriptBin "harness" ''
        set -u
        template=${lib.escapeShellArg settingsFile}
        live=${lib.escapeShellArg liveConfigPath}
        keyFile=${lib.escapeShellArg cfg.llmApiKeyFile}
        runtimeDir="''${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
        out="$runtimeDir/harness/config.toml"
        mkdir -p "$(dirname "$out")" "$(dirname "$live")"

        # sed-escape a replacement value (backslash, `&` and the `|` delimiter).
        esc() { printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'; }

        # Carry the last persisted `/model` choice from the sidecar onto the
        # fresh template (the line is the durable record; only `model` moves).
        prev=""
        if [ -f "$live" ]; then
          prev="$(sed -n 's/^[[:space:]]*model[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$live" | tail -n 1)"
        fi
        content="$(cat "$template")" || exit 1
        if [ -n "$prev" ]; then
          p_esc="$(esc "$prev")"
          content="$(printf '%s\n' "$content" | sed "0,/^[[:space:]]*model[[:space:]]*=[[:space:]]*.*$/s||model = \"$p_esc\"|")"
        fi

        # Key into a 0600 tmpfs copy, unchanged in posture.
        key="$(cat "$keyFile")" || exit 1
        content="''${content//${apiKeyPlaceholder}/$key}"
        umask 077
        printf '%s' "$content" > "$out.tmp"
        mv -f "$out.tmp" "$out"

        # Run against the writable tmpfs copy so `/model` persists there.
        ${cfg.package}/bin/harness --config "$out" "$@"
        status=$?

        # Publish the chosen model onto the keyless template; the durable
        # sidecar never contains the key.
        if [ -f "$out" ]; then
          model="$(sed -n 's/^[[:space:]]*model[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$out" | tail -n 1)"
          if [ -n "$model" ]; then
            m_esc="$(esc "$model")"
            sed "0,/^[[:space:]]*model[[:space:]]*=[[:space:]]*.*$/s||model = \"$m_esc\"|" "$template" > "$live.tmp"
            mv -f "$live.tmp" "$live"
          fi
        fi
        exit $status
      ''
    else
      pkgs.writeShellScriptBin "harness" ''
        set -u
        template=${lib.escapeShellArg settingsFile}
        live=${lib.escapeShellArg liveConfigPath}
        mkdir -p "$(dirname "$live")"

        # Regenerate the live file from the template each launch, so Nix
        # changes propagate; keep the previous `/model` choice, which the
        # app owns and rewrites in place.
        prev=""
        if [ -f "$live" ]; then
          prev="$(sed -n 's/^[[:space:]]*model[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$live" | tail -n 1)"
        fi
        content="$(cat "$template")" || exit 1
        if [ -n "$prev" ]; then
          p_esc="$(printf '%s' "$prev" | sed -e 's/[\\&|]/\\&/g')"
          content="$(printf '%s\n' "$content" | sed "0,/^[[:space:]]*model[[:space:]]*=[[:space:]]*.*$/s||model = \"$p_esc\"|")"
        fi
        printf '%s' "$content" > "$live.tmp" && mv -f "$live.tmp" "$live"

        exec ${cfg.package}/bin/harness --config "$live" "$@"
      '';

  installed =
    if hasSettings
    then wrapped
    else cfg.package;
in {
  options.programs.harness = {
    enable = lib.mkEnableOption "harness, the agent harness TUI";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      defaultText = lib.literalExpression "harness.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The `harness` binary to install. Set automatically when importing
        the flake's `homeManagerModules.default`; set it explicitly when
        importing `modules/harness.nix` directly.
      '';
    };

    llmApiKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/run/secrets/ninerouter_key";
      description = ''
        Runtime path to a file containing the LLM API key (e.g. a sops-nix
        secret under `/run/secrets`). Must be a plain string: a `path`
        value would be copied into the Nix store at eval time, defeating
        the point. When set, `settings.llm.api_key` is replaced by a
        placeholder before rendering (so it never reaches the store) and
        the wrapper substitutes the key into a `0600` copy under
        `$XDG_RUNTIME_DIR` that the app runs against; the persisted model
        is copied back to a keyless copy at the config path on exit.
      '';
    };

    settings = lib.mkOption {
      type = lib.types.attrsOf lib.types.anything;
      default = {};
      example = lib.literalExpression ''
        {
          llm = {
            base_url = "https://opencode.ai/zen/v1";
            model = "muse-spark-1.3-contributor-free";
            api_key = "public";
            user_agent = "opencode/1.18.18";
            headers = {
              HTTP-Referer = "https://opencode.ai/";
              X-Title = "opencode";
            };
            responses_models = [ "future-model-free" ];
          };
          agent.max_iterations = 128;
          session = {
            enabled = true;
            # `null`/omitted keys are left out of the file entirely, so
            # serde's `#[serde(default)]` applies.
            dir = null;
          };
          skills = {
            enabled = true;
            dir = "''${config.xdg.configHome}/harness/skills";
          };
          shell = {
            default_timeout_ms = 30000;
            max_timeout_ms = 120000;
            max_output_bytes = 65536;
            max_capture_bytes = 1048576;
            max_jobs = 32;
            max_job_time_ms = 600000;
            max_job_output_bytes = 262144;
          };
          mcp = {
            servers = {
              fetch = {
                command = "npx";
                args = [ "-y" "@modelcontextprotocol/server-fetch" ];
                timeout_ms = 30000;
              };
            };
          };
        }
      '';
      description = ''
        Contents of the config file, as an attrset mirroring the sections
        in `config.example.toml` (`llm`, `agent`, `session`, `skills`,
        `shell`, `mcp`).

        Required: `llm.base_url`, `llm.model` and `llm.api_key` have no
        serde defaults and a missing `[llm]` is a parse error, so `harness`
        exits at startup without them. Every other section defaults to
        the built-in values and can be left out. When `llmApiKeyFile` is
        set, `llm.api_key` here is replaced by a placeholder (use that
        option instead; the value here would never reach the store).

        Setting this installs the `--config` wrapper and a generated
        template; the file the app runs against is materialised from the
        template at launch (see the module header), so Nix stays the
        source of truth for every setting except `model`. Leave this at
        `{}` and no wrapper or file is created; `harness` then reads
        `$XDG_CONFIG_HOME/harness/config.toml` (else
        `$HOME/.config/harness/config.toml`, else `./config.toml`) on its
        own.
      '';
    };

    configFileName = lib.mkOption {
      type = lib.types.str;
      default = "config.toml";
      example = "harness.toml";
      description = ''
        File name the config is written under in the harness config
        directory. The wrapper passes the full path to `--config`, so
        any name works.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.package != null;
        message = "programs.harness.package must be set: the flake's homeManagerModules.default sets it, a direct import of modules/harness.nix must set it explicitly.";
      }
      {
        assertion = !useKeyFile || hasSettings;
        message = "programs.harness.llmApiKeyFile requires programs.harness.settings (the wrapper substitutes the key into the generated file).";
      }
    ];

    home.packages = lib.mkIf (cfg.package != null) [installed];

    # No `xdg.configFile` here: the config path is deliberately *not*
    # managed. The app rewrites `model` in place (`ConfigService::save`)
    # and a store symlink is read-only, while home-manager would clobber
    # a writable file on every switch. The wrapper materialises the live
    # file from the store template instead (see `wrapped`), so a switch
    # rolls every setting except the app-owned `model`.
  };
}
