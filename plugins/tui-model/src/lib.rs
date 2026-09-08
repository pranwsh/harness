//! Model selector popup: the `/model` bridge between the generic popup
//! surface and the model catalog.
//!
//! The TUI shell stays domain-free: it renders whatever
//! [`Popup`](harness_tui_popup::Popup) holds and routes keys here. This
//! plugin owns everything model-specific — the `/model` input trigger,
//! arrow/Enter/Esc handling while open, the background `/models` refresh,
//! and persisting the choice to `config.toml`.
//!
//! Provided as `Arc<ModelPopup>` under
//! [`KEY_MODEL_POPUP`](harness_contracts::KEY_MODEL_POPUP). Injects the
//! shared [`Popup`](harness_tui_popup::Popup) under
//! [`KEY_POPUP`](harness_contracts::KEY_POPUP) plus the selector; the config
//! service is optional so embedded/test contexts degrade to session-only
//! switches.

use std::sync::Arc;

use harness_agent_default_model::ModelSelector;
use harness_config::ConfigService;
use harness_contracts::{KEY_MODEL_POPUP, KEY_MODEL_SELECTOR, KEY_POPUP};
use harness_core::{Context, Result};
use harness_tui_popup::{ActivePopup, Popup};
use harness_tui_state::app::{App, AppMsg, KeyEvent};

/// Exact input line (trimmed) that opens the model popup instead of
/// submitting as chat.
pub const TRIGGER: &str = "/model";

/// Popup title, with the surrounding spaces the border title expects.
pub const TITLE: &str = " model ";

pub struct ModelPopup {
    popup: Arc<Popup>,
    selector: Arc<ModelSelector>,
    config_service: Option<Arc<ConfigService>>,
}

impl ModelPopup {
    pub fn new(
        popup: Arc<Popup>,
        selector: Arc<ModelSelector>,
        config_service: Option<Arc<ConfigService>>,
    ) -> Self {
        ModelPopup {
            popup,
            selector,
            config_service,
        }
    }

    /// Whether an input line should open the model popup instead of
    /// submitting as chat.
    pub fn wants_input(&self, input: &str) -> bool {
        input.trim() == TRIGGER
    }

    pub fn is_open(&self) -> bool {
        self.popup.is_open()
    }

    pub fn snapshot(&self) -> Option<ActivePopup> {
        self.popup.snapshot()
    }

    /// Opens the popup for an exact `/model` Enter: clears the command
    /// line, shows the cached catalog instantly, then refreshes
    /// `GET {base_url}/models` in the background without blocking input.
    pub fn open(&self, app: &mut App) {
        let current = self.selector.current();
        let models = self.selector.models();
        let selected = models.iter().position(|m| *m == current).unwrap_or(0);
        app.clear_input();
        self.popup.open(TITLE, models, selected, Some(current));
        let selector = self.selector.clone();
        let popup = self.popup.clone();
        tokio::spawn(async move {
            if let Some(ids) = selector.refresh().await {
                popup.refresh_items(ids, Some(selector.current()));
            }
        });
    }

    /// Handles one key while the popup is open. Returns `true` when the key
    /// belonged to the popup (consumed; never reaches `App`). `Up`/`Down`
    /// move the cursor, `Esc` dismisses, `Enter` applies the highlighted
    /// model (persisted to `config.toml` when a file-backed config service
    /// is present, rolled back otherwise) and resumes normal input.
    /// Anything else is swallowed so typing can't leak into the cleared
    /// editor mid-select. `Interrupt` (Ctrl+C) returns `false` so the app
    /// can still quit.
    pub fn handle_key(&self, key: KeyEvent, app: &mut App) -> bool {
        match key {
            KeyEvent::Up => {
                self.popup.move_up();
                true
            }
            KeyEvent::Down => {
                self.popup.move_down();
                true
            }
            KeyEvent::Esc => {
                self.popup.close();
                true
            }
            KeyEvent::Enter => {
                if let Some(model) = self.popup.selected_item() {
                    let notice = self.apply_selection(&model);
                    app.update(AppMsg::Notice(notice));
                }
                self.popup.close();
                true
            }
            KeyEvent::Interrupt => false,
            _ => true,
        }
    }

    /// Applies a popup model choice across the selector and `config.toml`.
    ///
    /// Pure orchestration (no terminal, no channel): in-memory first via
    /// `set_current`, then `ConfigService::set_llm_model` (itself
    /// transactional). Returns the notice text for the chat pane:
    /// - `model → {m} (saved)` on success (selector default follows),
    /// - `model → {m} (session only)` with no service (embedded/test configs),
    /// - `✗ config save failed: … (kept {prev})` with the selector restored
    ///   verbatim when the file write fails, so memory always matches the file
    ///   after success and is untouched after failure.
    pub fn apply_selection(&self, model: &str) -> String {
        let (prev_current, prev_catalog) = self.selector.snapshot();
        self.selector.set_current(model);
        let Some(service) = self.config_service.as_deref() else {
            return format!("model → {model} (session only)");
        };
        match service.set_llm_model(model) {
            Ok(_) => {
                self.selector.sync_default(model);
                format!("model → {model} (saved)")
            }
            Err(err) => {
                self.selector.restore(prev_current.clone(), prev_catalog);
                format!("✗ config save failed: {err} (kept {prev_current})")
            }
        }
    }
}

/// Plugin providing the model popup bridge as `Arc<ModelPopup>` under
/// [`KEY_MODEL_POPUP`](harness_contracts::KEY_MODEL_POPUP).
pub struct TuiModelPlugin;

impl harness_core::Plugin for TuiModelPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui-model")
            .provides(KEY_MODEL_POPUP)
            .injects(KEY_POPUP)
            .injects(KEY_MODEL_SELECTOR)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let popup: Arc<Popup> = ctx.inject_key(KEY_POPUP)?;
        let selector: Arc<ModelSelector> = ctx.inject_key(KEY_MODEL_SELECTOR)?;
        // Optional on purpose (no `injects` declaration, so parking behavior
        // is unchanged): embedded/test contexts without a file-backed config
        // degrade to session-only model switches.
        let config_service: Option<Arc<ConfigService>> =
            ctx.try_inject_key(harness_contracts::KEY_CONFIG_SERVICE);
        ctx.provide_key(
            KEY_MODEL_POPUP,
            Arc::new(ModelPopup::new(popup, selector, config_service)),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_tui_state::app::AppMsg;

    fn test_config(model: &str) -> harness_config::AppConfig {
        harness_config::AppConfig::from_toml(
            &format!(
                "[llm]\nbase_url = \"u\"\nmodel = \"{model}\"\napi_key = \"k\"\nuser_agent = \"a\"\n"
            ),
            "test",
        )
        .unwrap()
    }

    fn test_selector(model: &str) -> Arc<ModelSelector> {
        Arc::new(ModelSelector::new(harness_core::Context::root(), model))
    }

    fn test_popup(
        selector: Arc<ModelSelector>,
        config_service: Option<Arc<ConfigService>>,
    ) -> ModelPopup {
        ModelPopup::new(Arc::new(Popup::new()), selector, config_service)
    }

    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "harness-tui-model-test-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saved_selection_persists_and_syncs_default() {
        let dir = unique_tmp_dir("saved");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# keep\n[llm]\nbase_url = \"u\"\nmodel = \"m1\"\napi_key = \"k\"\nuser_agent = \"a\"\n",
        )
        .unwrap();
        let selector = test_selector("m1");
        let service = ConfigService::new(Some(path.clone()), test_config("m1"));
        let model_popup = test_popup(selector.clone(), Some(Arc::new(service)));

        let notice = model_popup.apply_selection("m2");

        assert_eq!(notice, "model → m2 (saved)");
        assert_eq!(selector.current(), "m2");
        assert_eq!(selector.default_model(), "m2");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("model = \"m2\""), "{raw:?}");
        assert!(raw.contains("# keep"), "{raw:?}");
    }

    #[test]
    fn failed_save_rolls_selector_back() {
        let dir = unique_tmp_dir("failed");
        let selector = test_selector("m1");
        // Points at a file that does not exist: the read fails.
        let service = ConfigService::new(Some(dir.join("absent.toml")), test_config("m1"));
        let model_popup = test_popup(selector.clone(), Some(Arc::new(service)));

        let notice = model_popup.apply_selection("m2");

        assert!(notice.contains("kept m1"), "{notice:?}");
        assert_eq!(
            selector.snapshot(),
            ("m1".to_owned(), vec!["m1".to_owned()])
        );
    }

    #[test]
    fn no_service_degrades_to_session_only() {
        let selector = test_selector("m1");
        let model_popup = test_popup(selector.clone(), None);

        let notice = model_popup.apply_selection("m2");

        assert_eq!(notice, "model → m2 (session only)");
        assert_eq!(selector.current(), "m2");
    }

    #[test]
    fn wants_input_matches_exact_trigger_only() {
        let model_popup = test_popup(test_selector("m1"), None);
        assert!(model_popup.wants_input("/model"));
        assert!(model_popup.wants_input("  /model  "));
        assert!(!model_popup.wants_input("/model x"));
        assert!(!model_popup.wants_input("/models"));
        assert!(!model_popup.wants_input("hello"));
    }

    #[tokio::test]
    async fn open_clears_input_and_shows_catalog() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into(), "m2".into()]);
        let model_popup = test_popup(selector, None);
        let mut app = App::new();
        for ch in "/model".chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
        assert_eq!(app.input(), "/model");

        model_popup.open(&mut app);

        assert_eq!(app.input(), "");
        assert!(model_popup.is_open());
        let snap = model_popup.snapshot().expect("open");
        assert_eq!(snap.items, vec!["m1".to_owned(), "m2".to_owned()]);
        assert_eq!(snap.current.as_deref(), Some("m1"));
    }

    #[tokio::test]
    async fn popup_keys_move_dismiss_and_apply() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into(), "m2".into()]);
        let model_popup = test_popup(selector.clone(), None);
        let mut app = App::new();
        model_popup.open(&mut app);

        assert!(model_popup.handle_key(KeyEvent::Down, &mut app));
        assert!(model_popup.handle_key(KeyEvent::Up, &mut app));
        // Swallowed: typing can't leak into the cleared editor mid-select.
        assert!(model_popup.handle_key(KeyEvent::Char('x'), &mut app));
        assert_eq!(app.input(), "");
        // Ctrl+C still quits via the app.
        assert!(!model_popup.handle_key(KeyEvent::Interrupt, &mut app));

        assert!(model_popup.handle_key(KeyEvent::Esc, &mut app));
        assert!(!model_popup.is_open());

        model_popup.open(&mut app);
        assert!(model_popup.handle_key(KeyEvent::Down, &mut app));
        assert!(model_popup.handle_key(KeyEvent::Enter, &mut app));
        assert!(!model_popup.is_open());
        assert_eq!(selector.current(), "m2");
        let last = app.items().last().expect("notice pushed");
        assert_eq!(last.text, "model → m2 (session only)");
    }

    #[test]
    fn plugin_provides_model_popup_through_di() {
        let ctx = Context::root();
        ctx.load(harness_tui_popup::TuiPopupPlugin).unwrap();
        ctx.load(
            harness_config::ConfigPlugin::from_toml(
                "[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n",
            )
            .unwrap(),
        )
        .unwrap();
        ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
            .unwrap();
        ctx.load(TuiModelPlugin).unwrap();
        let model_popup: Arc<ModelPopup> = ctx.inject_key(KEY_MODEL_POPUP).unwrap();
        assert!(!model_popup.is_open());
        assert!(model_popup.wants_input("/model"));
    }

    #[test]
    fn plugin_parks_until_popup_and_selector_are_loaded() {
        let ctx = Context::root();
        let outcome = ctx.load(TuiModelPlugin).unwrap();
        assert!(matches!(outcome, harness_core::LoadOutcome::Pending { .. }));

        ctx.load(harness_tui_popup::TuiPopupPlugin).unwrap();
        // Still parked: the selector is missing.
        assert!(ctx.provider_of(&KEY_MODEL_POPUP.into()).is_none());

        ctx.load(
            harness_config::ConfigPlugin::from_toml(
                "[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n",
            )
            .unwrap(),
        )
        .unwrap();
        ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
            .unwrap();
        assert!(ctx.provider_of(&KEY_MODEL_POPUP.into()).is_some());
    }
}
