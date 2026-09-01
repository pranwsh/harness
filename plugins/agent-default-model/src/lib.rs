use std::sync::Arc;

use harness_contracts::{CH_MODEL_SELECTED, KEY_CONFIG, KEY_MODEL_SELECTOR, ModelSelected};
use harness_core::{Context, Result};

use harness_config::AppConfig;

/// Chooses which model an agent uses.
///
/// v1 policy: the configured default for every agent. The seam exists so a
/// later plugin can swap in per-agent routing without touching the loop.
pub struct ModelSelector {
    ctx: Context,
    default: String,
}

impl ModelSelector {
    pub fn new(ctx: Context, default: impl Into<String>) -> Self {
        ModelSelector {
            ctx,
            default: default.into(),
        }
    }

    /// Returns the model for an agent, emitting `model.selected` for
    /// debugging/telemetry.
    pub fn select(&self, agent_id: &str) -> String {
        let model = self.default.clone();
        let _ = self.ctx.emit_key_detached(
            CH_MODEL_SELECTED,
            ModelSelected {
                agent_id: agent_id.to_owned(),
                model: model.clone(),
            },
        );
        model
    }
}

pub struct AgentDefaultModelPlugin;

impl harness_core::Plugin for AgentDefaultModelPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("agent-default-model")
            .provides(KEY_MODEL_SELECTOR)
            .injects(KEY_CONFIG)
            .emits::<ModelSelected>(CH_MODEL_SELECTED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let config: Arc<AppConfig> = ctx.inject_key(KEY_CONFIG)?;
        let selector = ModelSelector::new(ctx.clone(), config.llm.model.clone());
        ctx.provide_key(KEY_MODEL_SELECTOR, Arc::new(selector));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::KEY_AGENTS;
    use harness_core::Plugin;

    fn load_all(ctx: &Context) {
        ctx.load(
            harness_config::ConfigPlugin::from_toml(
                "[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n",
            )
            .unwrap(),
        )
        .unwrap();
        ctx.load(AgentDefaultModelPlugin).unwrap();
    }

    #[test]
    fn select_returns_configured_default() {
        let ctx = Context::root();
        load_all(&ctx);
        let sel: Arc<ModelSelector> = ctx.inject_key(KEY_MODEL_SELECTOR).unwrap();
        assert_eq!(sel.select("agent-1"), "m");
        assert_eq!(sel.select("agent-2"), "m");
    }

    #[test]
    fn parks_until_config_is_loaded() {
        let ctx = Context::root();
        let outcome = ctx.load(AgentDefaultModelPlugin).unwrap();
        assert!(matches!(outcome, harness_core::LoadOutcome::Pending { .. }));

        // Loading config cascades the parked plugin in.
        ctx.load(
            harness_config::ConfigPlugin::from_toml(
                "[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n",
            )
            .unwrap(),
        )
        .unwrap();
        assert!(ctx.provider_of(&KEY_MODEL_SELECTOR.into()).is_some());
        let sel: Arc<ModelSelector> = ctx.inject_key(KEY_MODEL_SELECTOR).unwrap();
        assert_eq!(sel.select("agent-1"), "m");
        let _ = KEY_AGENTS;
    }
}
