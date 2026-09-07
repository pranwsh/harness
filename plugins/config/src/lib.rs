use std::sync::Arc;

use harness_contracts::KEY_CONFIG;
use harness_core::{Context, Plugin, PluginMeta};
use serde::Deserialize;
use thiserror::Error;

pub const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// Fully parsed application configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AppConfig {
    pub llm: LlmConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub shell: ShellConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub user_agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentConfig {
    /// Hard cap on model iterations per turn.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            max_iterations: default_max_iterations(),
        }
    }
}

fn default_max_iterations() -> u32 {
    8
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionConfig {
    /// Storage backend; only "memory" is supported in v1.
    #[serde(default = "default_session_backend")]
    pub backend: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            backend: default_session_backend(),
        }
    }
}

fn default_session_backend() -> String {
    "memory".to_owned()
}

/// `[shell]` section: resource bounds only. No policy guardrails live here:
/// policy (allowlist/denylist/workdir/env filtering) is enforced by a future
/// guardrail plugin via the `tool.approval` waterfall, not by the shell.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ShellConfig {
    /// Sync default when the call omits `timeout_ms`.
    #[serde(default = "default_shell_timeout_ms")]
    pub default_timeout_ms: u64,
    /// Hard clamp for per-call `timeout_ms`.
    #[serde(default = "default_shell_max_timeout_ms")]
    pub max_timeout_ms: u64,
    /// Tail bytes kept per stream in formatted output.
    #[serde(default = "default_shell_max_output_bytes")]
    pub max_output_bytes: usize,
    /// Hard per-stream pipe cap while draining (bounds memory).
    #[serde(default = "default_shell_max_capture_bytes")]
    pub max_capture_bytes: usize,
    /// Max concurrent background jobs.
    #[serde(default = "default_shell_max_jobs")]
    pub max_jobs: usize,
    /// Max lifetime of one background job.
    #[serde(default = "default_shell_max_job_time_ms")]
    pub max_job_time_ms: u64,
    /// Ring-buffer cap per stream per background job.
    #[serde(default = "default_shell_max_job_output_bytes")]
    pub max_job_output_bytes: usize,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig {
            default_timeout_ms: default_shell_timeout_ms(),
            max_timeout_ms: default_shell_max_timeout_ms(),
            max_output_bytes: default_shell_max_output_bytes(),
            max_capture_bytes: default_shell_max_capture_bytes(),
            max_jobs: default_shell_max_jobs(),
            max_job_time_ms: default_shell_max_job_time_ms(),
            max_job_output_bytes: default_shell_max_job_output_bytes(),
        }
    }
}

fn default_shell_timeout_ms() -> u64 {
    30_000
}

fn default_shell_max_timeout_ms() -> u64 {
    120_000
}

fn default_shell_max_output_bytes() -> usize {
    65_536
}

fn default_shell_max_capture_bytes() -> usize {
    1_048_576
}

fn default_shell_max_jobs() -> usize {
    32
}

fn default_shell_max_job_time_ms() -> u64 {
    600_000
}

fn default_shell_max_job_output_bytes() -> usize {
    262_144
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("config file {path} is empty")]
    Empty { path: String },
}

impl AppConfig {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::from_toml(&raw, path)
    }

    pub fn from_toml(raw: &str, path: &str) -> Result<Self, ConfigError> {
        if raw.trim().is_empty() {
            return Err(ConfigError::Empty {
                path: path.to_owned(),
            });
        }
        toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
    }
}

/// Resolves the config path: `$HARNESS_CONFIG` or `config.toml` in cwd.
pub fn config_path() -> String {
    std::env::var("HARNESS_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_owned())
}

pub struct ConfigPlugin {
    config: Arc<AppConfig>,
}

impl ConfigPlugin {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        Ok(ConfigPlugin {
            config: Arc::new(AppConfig::from_file(path)?),
        })
    }

    /// Builds the plugin from raw TOML (tests and embedded configs).
    pub fn from_toml(raw: &str) -> Result<Self, ConfigError> {
        Ok(ConfigPlugin {
            config: Arc::new(AppConfig::from_toml(raw, "<embedded>")?),
        })
    }
}

impl Plugin for ConfigPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("config").provides(KEY_CONFIG)
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key(KEY_CONFIG, self.config.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = r#"
[llm]
base_url = "https://example.internal/v1"
model = "test-model"
api_key = "secret"
user_agent = "test/0.1"

[agent]
max_iterations = 3
"#;

    #[test]
    fn parses_full_config() {
        let cfg = AppConfig::from_toml(RAW, "test").unwrap();
        assert_eq!(cfg.llm.model, "test-model");
        assert_eq!(cfg.agent.max_iterations, 3);
        assert_eq!(cfg.session.backend, "memory");
    }

    #[test]
    fn defaults_kick_in_for_missing_sections() {
        let raw = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"
"#;
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.agent.max_iterations, 8);
        assert_eq!(cfg.session.backend, "memory");
    }

    #[test]
    fn rejects_empty_file() {
        assert!(matches!(
            AppConfig::from_toml("   \n", "test"),
            Err(ConfigError::Empty { .. })
        ));
    }

    #[test]
    fn rejects_missing_llm_section() {
        assert!(matches!(
            AppConfig::from_toml("[agent]\nmax_iterations = 2\n", "test"),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn shell_defaults_are_bounds_only() {
        let raw = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"
"#;
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.shell.default_timeout_ms, 30_000);
        assert_eq!(cfg.shell.max_timeout_ms, 120_000);
        assert_eq!(cfg.shell.max_jobs, 32);
        assert_eq!(cfg.shell.max_job_time_ms, 600_000);
    }

    #[test]
    fn shell_section_overrides() {
        let raw = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"

[shell]
default_timeout_ms = 5000
max_jobs = 4
"#;
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.shell.default_timeout_ms, 5000);
        assert_eq!(cfg.shell.max_jobs, 4);
    }

    #[test]
    fn shell_ignores_legacy_policy_keys() {
        // Pre-guardrail-removal configs with policy keys must still parse;
        // unknown fields are ignored, bounds apply.
        let raw = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"

[shell]
allowlist = ["git", "ls"]
allow_shell = true
default_timeout_ms = 5000
max_jobs = 4

[shell.env]
mode = "clean"
denied_patterns = ["CUSTOM_*"]
"#;
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.shell.default_timeout_ms, 5000);
        assert_eq!(cfg.shell.max_jobs, 4);
    }
}
