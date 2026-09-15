//! Model selector popup: the `/model` bridge between a filter popup and
//! the model catalog.
//!
//! The TUI shell stays domain-free: it draws at most one provider snapshot
//! (model first) and routes keys here while a search is active. This plugin
//! owns everything model-specific — the `/model` input trigger, the
//! `/model <query>` search syntax, substring match filtering, applying the
//! choice (persisted to `config.toml`), and the background `/models`
//! refresh. Listing behavior itself is the shared
//! [`FilterPopup`](harness_tui_filter::FilterPopup) driver, so the entry
//! bar stays live while searching, exactly like slash-command completion.
//!
//! Provided as `Arc<ModelPopup>` under
//! [`KEY_MODEL_POPUP`](harness_contracts::KEY_MODEL_POPUP). Injects only
//! the selector; the config service is optional so embedded/test contexts
//! degrade to session-only switches. Owns its popup surface, so it never
//! parks on — or observes — any other provider's list.

use std::sync::Arc;

use harness_contracts::{
    ConfigHandle, KEY_CONFIG, KEY_MODEL_CATALOG, KEY_MODEL_POPUP, ModelCatalogHandle,
};
use harness_core::{Context, Result};
use harness_tui_filter::{FilterPopup, FilterSource};
use harness_tui_popup::ActivePopup;
use harness_tui_state::app::{App, AppMsg, KeyEvent};

/// Exact input line (trimmed) that opens model search instead of
/// submitting as chat.
pub const TRIGGER: &str = "/model";

/// Popup title, with the surrounding spaces the border title expects.
pub const TITLE: &str = " model ";

/// Extracts the model-search query from an input line: `/model` alone or
/// `/model <query>` (single line). Returns `None` for anything else, which
/// ends the search session.
pub fn model_query(input: &str) -> Option<String> {
    let trimmed = input.trim_start();
    if trimmed.contains('\n') {
        return None;
    }
    let rest = trimmed.strip_prefix(TRIGGER)?;
    if rest.is_empty() {
        return Some(String::new());
    }
    match rest.chars().next() {
        Some(c) if c.is_whitespace() => Some(rest.trim().to_owned()),
        _ => None,
    }
}

/// Model source for the shared driver: `/model <query>` candidate,
/// case-insensitive substring matches, shared complete-unless-exact
/// `Enter`, and exact-`Enter` applies the highlight immediately.
#[derive(Clone)]
struct ModelSource {
    selector: Arc<ModelCatalogHandle>,
    config_service: Option<Arc<ConfigHandle>>,
}

impl FilterSource for ModelSource {
    fn title(&self) -> &str {
        TITLE
    }

    fn candidate(&self, input: &str) -> Option<String> {
        model_query(input)
    }

    fn items(&self, query: &str) -> Vec<String> {
        let query = query.to_lowercase();
        self.selector
            .models()
            .into_iter()
            .filter(|model| model.to_lowercase().contains(&query))
            .collect()
    }

    fn current(&self) -> Option<String> {
        Some(self.selector.current())
    }

    fn initial_selected(&self, items: &[String]) -> usize {
        let current = self.selector.current();
        items.iter().position(|m| *m == current).unwrap_or(0)
    }

    fn render_completion(&self, selected: &str) -> String {
        format!("{TRIGGER} {selected}")
    }

    fn on_dismiss(&self, app: &mut App) {
        app.clear_input();
    }

    fn on_exact(&self, selected: &str, app: &mut App) -> bool {
        // Exact `Enter` applies the highlight: a row outside the catalog
        // (unreachable on the owned surface) is rejected without touching
        // selector or `config.toml`.
        if self.selector.models().iter().any(|m| m == selected) {
            let notice = self.apply_selection(selected);
            app.update(AppMsg::Notice(notice));
        }
        app.clear_input();
        true
    }
}

impl ModelSource {
    /// Applies a model choice across the selector and `config.toml`. Pure
    /// orchestration (no terminal, no channel): in-memory first via
    /// `set_current`, then `ConfigService::set_llm_model` (itself
    /// transactional). Returns the notice text for the chat pane.
    fn apply_selection(&self, model: &str) -> String {
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

pub struct ModelPopup {
    filter: FilterPopup<ModelSource>,
    source: ModelSource,
}

impl ModelPopup {
    pub fn new(
        selector: Arc<ModelCatalogHandle>,
        config_service: Option<Arc<ConfigHandle>>,
    ) -> Self {
        let source = ModelSource {
            selector,
            config_service,
        };
        ModelPopup {
            filter: FilterPopup::new(source.clone()),
            source,
        }
    }

    /// Whether an input line should open model search instead of
    /// submitting as chat.
    pub fn wants_input(&self, input: &str) -> bool {
        input.trim() == TRIGGER
    }

    /// Whether a search session is live (independent of list visibility).
    /// The shell routes keys here — and suppresses slash completion —
    /// while this is true.
    pub fn is_active(&self) -> bool {
        self.filter.is_active()
    }

    pub fn is_open(&self) -> bool {
        self.filter.is_open()
    }

    pub fn snapshot(&self) -> Option<ActivePopup> {
        self.filter.snapshot()
    }

    /// Ends the session and hides the list.
    pub fn close(&self) {
        self.filter.close();
    }

    /// Opens model search for an exact `/model` Enter: stages `/model ` in
    /// the (still live) entry bar, shows the cached catalog instantly with
    /// the cursor on the current model, then refreshes
    /// `GET {base_url}/models` in the background without blocking input.
    pub fn open(&self, app: &mut App) {
        app.set_input(&format!("{TRIGGER} "));
        self.filter.sync(app);
        let selector = self.source.selector.clone();
        let popup = self.filter.popup();
        tokio::spawn(async move {
            if let Some(ids) = std::sync::Arc::clone(&selector.0).refresh().await {
                popup.refresh_items(ids, Some(selector.current()));
            }
        });
    }

    /// Reconciles the search with the input line. No-op unless a session
    /// is live; editing away from the `/model` prefix ends it (the shell
    /// then hands the line back to slash completion).
    pub fn sync(&self, app: &App) {
        if !self.filter.is_active() {
            return;
        }
        self.filter.sync(app);
    }

    /// Handles one key via the shared driver. Returns `true` when consumed
    /// (never reaches `App`); editing keys fall through so the entry bar
    /// stays live and re-filters via the shell's post-reduce `sync`.
    /// `Interrupt` (Ctrl+C) returns `false` so the app can still quit.
    pub fn handle_key(&self, key: KeyEvent, app: &mut App) -> bool {
        if !self.filter.is_active() {
            return false;
        }
        self.filter.handle_key(key, app)
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
        self.source.apply_selection(model)
    }
}

/// Plugin providing the model popup bridge as `Arc<ModelPopup>` under
/// [`KEY_MODEL_POPUP`](harness_contracts::KEY_MODEL_POPUP).
pub struct TuiModelPlugin;

impl harness_core::Plugin for TuiModelPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui-model")
            .provides(KEY_MODEL_POPUP)
            .injects(KEY_MODEL_CATALOG)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let selector: Arc<ModelCatalogHandle> = ctx.inject_key(KEY_MODEL_CATALOG)?;
        // Optional on purpose (no `injects` declaration, so parking behavior
        // is unchanged): embedded/test contexts without a file-backed config
        // degrade to session-only model switches.
        let config_service: Option<Arc<ConfigHandle>> = ctx.try_inject_key(KEY_CONFIG);
        ctx.provide_key(
            KEY_MODEL_POPUP,
            Arc::new(ModelPopup::new(selector, config_service)),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_config::ConfigService;
    use harness_contracts::{ConfigHandle, ModelCatalogHandle};
    use harness_tui_state::app::AppMsg;

    fn test_config(model: &str) -> harness_contracts::AppConfig {
        harness_contracts::AppConfig::from_toml(
            &format!(
                "[llm]\nbase_url = \"u\"\nmodel = \"{model}\"\napi_key = \"k\"\nuser_agent = \"a\"\n"
            ),
            "test",
        )
        .unwrap()
    }

    fn test_selector(model: &str) -> Arc<ModelCatalogHandle> {
        use harness_agent_default_model::ModelSelector;
        use harness_contracts::ModelCatalogApi;
        let sel = Arc::new(ModelSelector::new(harness_core::Context::root(), model));
        Arc::new(ModelCatalogHandle(sel as Arc<dyn ModelCatalogApi>))
    }

    fn test_popup(
        selector: Arc<ModelCatalogHandle>,
        config_service: Option<Arc<ConfigService>>,
    ) -> ModelPopup {
        let wrapped = config_service
            .map(|svc| Arc::new(ConfigHandle(svc as Arc<dyn harness_contracts::ConfigApi>)));
        ModelPopup::new(selector, wrapped)
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

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
    }

    /// Drives one shell-style key: falls through to `App` unless the
    /// search consumes it, then re-syncs the eligible provider state.
    fn press(model_popup: &ModelPopup, app: &mut App, key: KeyEvent) {
        if !model_popup.handle_key(key, app) {
            app.update(AppMsg::Key(key));
        }
        model_popup.sync(app);
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

    #[test]
    fn model_query_parses_search_syntax() {
        assert_eq!(model_query("/model"), Some(String::new()));
        assert_eq!(model_query("/model "), Some(String::new()));
        assert_eq!(model_query("  /model  "), Some(String::new()));
        assert_eq!(model_query("/model gpt"), Some("gpt".to_owned()));
        assert_eq!(model_query("/model  gpt  "), Some("gpt".to_owned()));
        assert_eq!(model_query("/modelx"), None);
        assert_eq!(model_query("/modelx y"), None);
        assert_eq!(model_query("/models"), None);
        assert_eq!(model_query("hello"), None);
        assert_eq!(model_query(""), None);
        assert_eq!(model_query("/model\ngpt"), None);
    }

    #[tokio::test]
    async fn open_stages_search_line_and_shows_catalog() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into(), "m2".into()]);
        let model_popup = test_popup(selector, None);
        let mut app = App::new();
        for ch in "/model".chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
        assert_eq!(app.input(), "/model");

        model_popup.open(&mut app);

        // Entry bar stays live with the staged trigger (never cleared).
        assert_eq!(app.input(), "/model ");
        assert!(model_popup.is_active());
        assert!(model_popup.is_open());
        let snap = model_popup.snapshot().expect("open");
        assert_eq!(snap.items, vec!["m1".to_owned(), "m2".to_owned()]);
        // Cursor starts on the current model.
        assert_eq!(snap.selected, 0);
        assert_eq!(snap.current.as_deref(), Some("m1"));
    }

    #[tokio::test]
    async fn typing_filters_and_entry_stays_live() {
        let selector = test_selector("openai/gpt-4o");
        selector.set_catalog(vec![
            "openai/gpt-4o".into(),
            "openai/gpt-4o-mini".into(),
            "anthropic/claude".into(),
        ]);
        let model_popup = test_popup(selector, None);
        let mut app = App::new();
        model_popup.open(&mut app);

        // Typing falls through to the app (never swallowed), then narrows.
        for ch in "ClAuDe".chars() {
            press(&model_popup, &mut app, KeyEvent::Char(ch));
        }
        // Case-insensitive substring match.
        assert_eq!(app.input(), "/model ClAuDe");
        let snap = model_popup.snapshot().expect("open");
        assert_eq!(snap.items, vec!["anthropic/claude".to_owned()]);

        // Backspacing widens again.
        for _ in 0..6 {
            press(&model_popup, &mut app, KeyEvent::Backspace);
        }
        assert_eq!(app.input(), "/model ");
        assert_eq!(model_popup.snapshot().expect("open").items.len(), 3);
    }

    #[tokio::test]
    async fn arrows_move_and_enter_completes_then_applies() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into(), "m2".into()]);
        let model_popup = test_popup(selector.clone(), None);
        let mut app = App::new();
        model_popup.open(&mut app);

        assert!(model_popup.handle_key(KeyEvent::Down, &mut app));
        assert_eq!(model_popup.snapshot().expect("open").selected, 1);
        assert!(model_popup.handle_key(KeyEvent::Up, &mut app));
        // Ctrl+C still quits via the app.
        assert!(!model_popup.handle_key(KeyEvent::Interrupt, &mut app));

        // First Enter completes the highlight into the input bar (no apply).
        assert!(model_popup.handle_key(KeyEvent::Down, &mut app));
        assert!(model_popup.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(app.input(), "/model m2");
        assert_eq!(selector.current(), "m1");
        assert!(model_popup.is_active());
        assert_eq!(
            model_popup.snapshot().expect("open").items,
            vec!["m2".to_owned()]
        );

        // Second Enter is exact: applies with a notice and clears.
        assert!(model_popup.handle_key(KeyEvent::Enter, &mut app));
        assert!(!model_popup.is_active());
        assert!(!model_popup.is_open());
        assert_eq!(app.input(), "");
        assert_eq!(selector.current(), "m2");
        let last = app.items().last().expect("notice pushed");
        assert_eq!(last.text, "model → m2 (session only)");
    }

    #[tokio::test]
    async fn enter_on_partial_query_completes_without_applying() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into(), "m2".into()]);
        let model_popup = test_popup(selector.clone(), None);
        let mut app = App::new();
        model_popup.open(&mut app);
        press(&model_popup, &mut app, KeyEvent::Char('2'));
        assert_eq!(app.input(), "/model 2");

        assert!(model_popup.handle_key(KeyEvent::Enter, &mut app));

        assert_eq!(app.input(), "/model m2");
        assert_eq!(selector.current(), "m1");
        assert!(app.items().is_empty(), "no notice pushed");
    }

    #[tokio::test]
    async fn tab_completes_id_and_enter_applies() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into(), "m2".into()]);
        let model_popup = test_popup(selector.clone(), None);
        let mut app = App::new();
        model_popup.open(&mut app);
        press(&model_popup, &mut app, KeyEvent::Char('2'));

        assert_eq!(
            model_popup.snapshot().expect("open").items,
            vec!["m2".to_owned()]
        );
        assert!(model_popup.handle_key(KeyEvent::Tab, &mut app));
        assert_eq!(app.input(), "/model m2");

        assert!(model_popup.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(selector.current(), "m2");
        assert_eq!(app.input(), "");
        assert!(!model_popup.is_active());
    }

    #[tokio::test]
    async fn esc_dismisses_and_clears_search_line() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into(), "m2".into()]);
        let model_popup = test_popup(selector.clone(), None);
        let mut app = App::new();
        model_popup.open(&mut app);
        type_text(&mut app, "m2");
        model_popup.sync(&app);
        assert!(model_popup.is_open());

        assert!(model_popup.handle_key(KeyEvent::Esc, &mut app));

        assert!(!model_popup.is_active());
        assert!(!model_popup.is_open());
        assert_eq!(app.input(), "");
        assert_eq!(selector.current(), "m1");
    }

    #[tokio::test]
    async fn editing_away_ends_session() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into(), "m2".into()]);
        let model_popup = test_popup(selector, None);
        let mut app = App::new();
        model_popup.open(&mut app);
        assert!(model_popup.is_active());

        // Backspacing below the `/model` prefix ends the search so the
        // shell can hand the line back to slash completion.
        for _ in 0..2 {
            press(&model_popup, &mut app, KeyEvent::Backspace);
        }
        assert_eq!(app.input(), "/mode");
        assert!(!model_popup.is_active());
        assert!(!model_popup.is_open());
    }

    #[tokio::test]
    async fn enter_with_no_match_falls_through() {
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into()]);
        let model_popup = test_popup(selector, None);
        let mut app = App::new();
        model_popup.open(&mut app);
        type_text(&mut app, "zzz");
        model_popup.sync(&app);
        // List hidden but session live; Enter has nothing to apply.
        assert!(model_popup.is_active());
        assert!(!model_popup.is_open());
        assert!(!model_popup.handle_key(KeyEvent::Enter, &mut app));
    }

    #[tokio::test]
    async fn sync_without_session_is_noop() {
        // Typing `/model` must never auto-open search: sessions start
        // only via an explicit `open`.
        let selector = test_selector("m1");
        selector.set_catalog(vec!["m1".into()]);
        let model_popup = test_popup(selector, None);
        let mut app = App::new();
        type_text(&mut app, "/model");
        model_popup.sync(&app);
        assert!(!model_popup.is_active());
        assert!(!model_popup.is_open());
    }

    #[test]
    fn plugin_provides_model_popup_through_di() {
        let ctx = Context::root();
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
        assert!(!model_popup.is_active());
        assert!(model_popup.wants_input("/model"));
    }

    #[test]
    fn plugin_parks_until_selector_is_loaded() {
        let ctx = Context::root();
        let outcome = ctx.load(TuiModelPlugin).unwrap();
        assert!(matches!(outcome, harness_core::LoadOutcome::Pending { .. }));
        assert!(ctx.provider_of(&KEY_MODEL_POPUP.into()).is_none());

        ctx.load(
            harness_config::ConfigPlugin::from_toml(
                "[llm]\nbase_url=\"u\"\nmodel=\"m\"\napi_key=\"k\"\nuser_agent=\"a\"\n",
            )
            .unwrap(),
        )
        .unwrap();
        // Still parked: the selector is missing (config is optional).
        assert!(ctx.provider_of(&KEY_MODEL_POPUP.into()).is_none());

        ctx.load(harness_agent_default_model::AgentDefaultModelPlugin)
            .unwrap();
        assert!(ctx.provider_of(&KEY_MODEL_POPUP.into()).is_some());
    }
}
