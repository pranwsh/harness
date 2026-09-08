use std::sync::{Arc, RwLock};

use harness_contracts::{CH_MODEL_SELECTED, KEY_CONFIG, KEY_MODEL_SELECTOR, ModelSelected};
use harness_core::{Context, Result};

use harness_config::AppConfig;

/// Chooses which model an agent uses.
///
/// Simple and UI-free: holds the configured default, an in-memory override
/// set through the `/model` popup, and a cached catalog (seeded with the default
/// so the popup opens instantly; refreshed from `GET {base_url}/models` in
/// the background). Knows nothing about popups or the TUI — the tui-model
/// plugin is the only bridge between this and the generic popup service.
pub struct ModelSelector {
    ctx: Context,
    default: RwLock<String>,
    current: RwLock<String>,
    catalog: RwLock<Vec<String>>,
    base_url: String,
    api_key: String,
}

impl ModelSelector {
    pub fn new(ctx: Context, default: impl Into<String>) -> Self {
        let default = default.into();
        ModelSelector {
            ctx,
            default: RwLock::new(default.clone()),
            current: RwLock::new(default.clone()),
            catalog: RwLock::new(vec![default]),
            base_url: String::new(),
            api_key: String::new(),
        }
    }

    pub fn from_config(ctx: Context, config: &AppConfig) -> Self {
        let default = config.llm.model.clone();
        ModelSelector {
            ctx,
            default: RwLock::new(default.clone()),
            current: RwLock::new(default.clone()),
            catalog: RwLock::new(vec![default]),
            base_url: config.llm.base_url.clone(),
            api_key: config.llm.api_key.clone(),
        }
    }

    /// Returns the model for an agent, emitting `model.selected` for
    /// debugging/telemetry.
    pub fn select(&self, agent_id: &str) -> String {
        let model = self.current();
        let _ = self.ctx.emit_key_detached(
            CH_MODEL_SELECTED,
            ModelSelected {
                agent_id: agent_id.to_owned(),
                model: model.clone(),
            },
        );
        model
    }

    /// Active model (override when set, else the configured default).
    pub fn current(&self) -> String {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn default_model(&self) -> String {
        self.default
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Cached catalog; at least the default is always present.
    pub fn models(&self) -> Vec<String> {
        self.catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Sets the active model and ensures it is listed in the catalog.
    pub fn set_current(&self, model: &str) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = model.to_owned();
        let mut catalog = self.catalog.write().unwrap_or_else(|e| e.into_inner());
        if !catalog.iter().any(|m| m == model) {
            catalog.push(model.to_owned());
            catalog.sort();
        }
    }

    /// Replaces the cached catalog (keeps the default listed). Used by the
    /// background `/models` refresh while the popup is open.
    pub fn set_catalog(&self, mut models: Vec<String>) {
        let default = self.default_model();
        if !models.iter().any(|m| m == &default) {
            models.push(default);
        }
        models.sort();
        models.dedup();
        *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = models;
    }

    /// Snapshot of `(current, catalog)` for orchestrated rollback when a
    /// downstream persist fails. See `restore`.
    pub fn snapshot(&self) -> (String, Vec<String>) {
        (self.current(), self.models())
    }

    /// Restores a `snapshot`, verbatim (no default re-anchoring, no sort).
    pub fn restore(&self, current: String, catalog: Vec<String>) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = current;
        *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = catalog;
    }

    /// Marks a successfully persisted model as the new default, so a later
    /// `set_catalog` fallback and a fresh process agree on it.
    pub fn sync_default(&self, model: &str) {
        *self.default.write().unwrap_or_else(|e| e.into_inner()) = model.to_owned();
    }

    /// Fetches `GET {base_url}/models` once and caches the result. Returns
    /// the fresh ids on success, `None` on any failure (caller keeps the
    /// cached catalog so the popup still works offline). Never panics.
    pub async fn refresh(&self) -> Option<Vec<String>> {
        let ids = fetch_model_ids(&self.base_url, &self.api_key).await?;
        if ids.is_empty() {
            return None;
        }
        self.set_catalog(ids.clone());
        Some(ids)
    }
}

/// One `GET {base_url}/models` round trip. Tolerates OpenAI
/// (`{data:[{id}]}`) and string-element shapes; empty vec on any failure.
async fn fetch_model_ids(base_url: &str, api_key: &str) -> Option<Vec<String>> {
    if base_url.is_empty() {
        return None;
    }
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = client.get(url).bearer_auth(api_key).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let value: serde_json::Value = resp.json().await.ok()?;
    let ids = parse_model_ids(&value);
    if ids.is_empty() { None } else { Some(ids) }
}

/// Pure parser, unit-tested without HTTP.
fn parse_model_ids(value: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| match item {
                    serde_json::Value::String(s) => Some(s.clone()),
                    _ => item.get("id").and_then(|id| id.as_str()).map(str::to_owned),
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    ids
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
        let selector = ModelSelector::from_config(ctx.clone(), &config);
        ctx.provide_key(KEY_MODEL_SELECTOR, Arc::new(selector));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::KEY_AGENTS;

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
    fn override_wins_and_catalog_keeps_default() {
        let ctx = Context::root();
        load_all(&ctx);
        let sel: Arc<ModelSelector> = ctx.inject_key(KEY_MODEL_SELECTOR).unwrap();
        assert_eq!(sel.current(), "m");
        assert_eq!(sel.models(), vec!["m"]);
        sel.set_current("other");
        assert_eq!(sel.current(), "other");
        assert_eq!(sel.select("agent-1"), "other");
        assert!(sel.models().contains(&"m".to_owned()));
        assert!(sel.models().contains(&"other".to_owned()));
    }

    #[test]
    fn set_catalog_sorts_dedups_and_keeps_default() {
        let ctx = Context::root();
        load_all(&ctx);
        let sel: Arc<ModelSelector> = ctx.inject_key(KEY_MODEL_SELECTOR).unwrap();
        sel.set_catalog(vec!["b".into(), "a".into(), "a".into()]);
        assert_eq!(sel.models(), vec!["a", "b", "m"]);
    }

    #[test]
    fn snapshot_restore_and_sync_default() {
        let ctx = Context::root();
        load_all(&ctx);
        let sel: Arc<ModelSelector> = ctx.inject_key(KEY_MODEL_SELECTOR).unwrap();
        let prev = sel.snapshot();
        sel.set_current("other");
        assert_eq!(sel.current(), "other");
        sel.restore(prev.0.clone(), prev.1.clone());
        assert_eq!(sel.snapshot(), prev);
        assert_eq!(sel.default_model(), "m");
        sel.set_current("other");
        sel.sync_default("other");
        assert_eq!(sel.default_model(), "other");
        // New default anchors future catalogs.
        sel.set_catalog(vec!["z".into()]);
        assert_eq!(sel.models(), vec!["other", "z"]);
    }

    #[test]
    fn parse_model_ids_handles_object_and_string_shapes() {
        let value = serde_json::json!({
            "data": [{"id": "b"}, "a", {"id": ""}, {"no": "x"}]
        });
        assert_eq!(parse_model_ids(&value), vec!["a", "b"]);
        assert!(parse_model_ids(&serde_json::json!({})).is_empty());
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
