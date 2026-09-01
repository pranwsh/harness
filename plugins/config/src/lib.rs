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
}
