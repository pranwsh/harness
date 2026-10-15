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
# Two things worth knowing before using it:
#
#   * The file is managed, i.e. a symlink into the Nix store, and the model
#     popup persists a choice by rewriting `[llm] model` in place
#     (`ConfigService::save`). Against a store symlink that write always
#     fails: the choice applies for the session and the notice reads
#     "config save failed". Expected, not a bug -- set `settings.llm.model`
#     and re-run `home-manager switch` to change it.
#   * `harness` resolves its config as `./config.toml` in the current
#     working directory, with no XDG fallback (see
#     `plugins/config/src/lib.rs`). It therefore cannot find a file managed
#     at `$XDG_CONFIG_HOME/harness/config.toml` on its own, so when
#     `settings` is set the module installs a wrapper that passes
#     `--config <managed path>` for you.
{
  config,
  lib,
  pkgs,
  self,
  ...
}: let
  cfg = config.programs.harness;
  toml = import ./toml.nix {inherit lib;};

  # Absolute target, so an `xdg.configHome` override is honoured instead of
  # writing to a hardcoded `~/.config`. Same directory the session/skills
  # plugins resolve their own dirs from.
  configPath = "${config.xdg.configHome}/harness/${cfg.configFileName}";

  settingsFile = lib.mkIf (cfg.enable && cfg.settings != {}) (
    pkgs.writeText cfg.configFileName (toml cfg.settings + "\n")
  );

  # Bare `harness` ignores a managed config (see header), so the package on
  # PATH is a wrapper whenever `settings` is set. `--config` is the only
  # flag the CLI accepts, and it takes a separate value, never `--config=PATH`.
  wrapped = lib.mkIf (cfg.enable && cfg.settings != {}) (
    pkgs.writeShellScriptBin "harness" ''
      exec ${cfg.package}/bin/harness --config ${lib.escapeShellArg configPath} "$@"
    ''
  );

  # `mkIf` yields `false`, and a conditional would then have to type-check
  # both arms, so select in `config` instead: the bare package when no
  # settings are managed, the wrapper when there are.
  installed =
    if cfg.settings == {}
    then cfg.package
    else wrapped;
in {
  options.programs.harness = {
    enable = lib.mkEnableOption "harness, the agent harness TUI";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "harness.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = ''
        The `harness` binary to install. Defaults to this flake's
        package for the host platform.
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
            dir = "${config.xdg.configHome}/harness/skills";
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
          theme = {
            colors = {
              "text.user_border" = "green";
              "text.error" = { fg = "red"; bg = "black"; };
              "markdown.emphasis" = "220";
            };
            code.name = "base16-eighties.dark";
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
        `shell`, `theme`, `mcp`).

        Required: `llm.base_url`, `llm.model` and `llm.api_key` have no
        serde defaults and a missing `[llm]` is a parse error, so `harness`
        exits at startup without them. Every other section defaults to
        the built-in values and can be left out.

        Setting this enables the generated file and the `--config`
        wrapper. Leave it at `{}` and neither is created, so `harness`
        keeps reading `./config.toml` from the cwd.
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
    home.packages = [installed];

    # `mkIf` keeps the file out of the generation entirely when `settings`
    # is unset, rather than linking an empty `config.toml` that fails to
    # parse (`ConfigError::Empty`).
    xdg.configFile."harness/${cfg.configFileName}" = lib.mkIf (cfg.settings != {}) {
      target = configPath;
      source = settingsFile;
    };
  };
}
