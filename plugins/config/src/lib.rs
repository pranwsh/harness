use std::sync::Arc;

use harness_core::{Context, Plugin, PluginMeta};
use serde::Deserialize;

pub const KEY_LLM_CONFIG: &str = "config.llm";
pub const DEFAULT_CONFIG_PATH: &str = "config.toml";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub user_agent: String,
}

impl LlmConfig {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
    }
}

#[derive(Debug, thiserror::Error)]
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
}

pub fn config_path() -> String {
    std::env::var("HARNESS_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_owned())
}

pub struct ConfigPlugin {
    config: Arc<LlmConfig>,
}

impl ConfigPlugin {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        Ok(ConfigPlugin {
            config: Arc::new(LlmConfig::from_file(path)?),
        })
    }
}

impl Plugin for ConfigPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("config-parser").provides(KEY_LLM_CONFIG)
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key(KEY_LLM_CONFIG, self.config.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_llm_config() {
        let raw = r#"
base_url = "https://example.test/v1"
model = "m1"
api_key = "k"
user_agent = "opencode/1.18.18"
"#;
        let config: LlmConfig = toml::from_str(raw).unwrap();
        assert_eq!(
            config,
            LlmConfig {
                base_url: "https://example.test/v1".into(),
                model: "m1".into(),
                api_key: "k".into(),
                user_agent: "opencode/1.18.18".into(),
            }
        );
    }

    #[test]
    fn rejects_missing_fields() {
        let raw = "base_url = \"https://example.test/v1\"\n";
        assert!(toml::from_str::<LlmConfig>(raw).is_err());
    }
}
