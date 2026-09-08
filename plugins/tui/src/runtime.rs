//! Runtime: owns the terminal, the main event loop, and the turn
//! forwarder tasks that bridge agent-loop streams into the app channel.

use std::sync::Arc;

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
use tokio::sync::mpsc;

use harness_agent_default_model::ModelSelector;
use harness_agent_loop::{AgentLoop, TurnEvent};
use harness_config::ConfigService;
use harness_tui_input::Input;
use harness_tui_popup::Popup;
use harness_tui_state::{
    app::{App, AppMsg, KeyEvent},
    render::RendererHandle,
};

use crate::view;

/// How often to redraw even without input, so the busy hint and
/// late-arriving stream events are always fresh.
const RENDER_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Runs the TUI until the app quits. Restores the terminal on exit,
/// including on panic (hooks below). Services come from DI so alternative
/// renderers or input sources plug in without touching this loop.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    renderer: Arc<RendererHandle>,
    input: Arc<Input>,
    popup: Arc<Popup>,
    selector: Arc<ModelSelector>,
    config_service: Option<Arc<ConfigService>>,
) {
    let terminal = ratatui::init();
    enable_terminal_features();
    let result = EventLoop::new(
        agent_loop,
        agent_id,
        session_id,
        renderer,
        input,
        popup,
        selector,
        config_service,
    )
    .run(terminal)
    .await;
    disable_terminal_features();
    ratatui::restore();
    if let Err(err) = result {
        eprintln!("tui: {err}");
    }
}

/// Mouse reporting (wheel scrolling) and Kitty keyboard enhancements
/// (disambiguated Shift+Enter). Best-effort: terminals that lack
/// either feature ignore the sequences.
fn enable_terminal_features() {
    let mut stdout = std::io::stdout();
    let _ = execute!(
        stdout,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    // ratatui::init installs a panic hook that restores the terminal;
    // chain after it so mouse reporting is dropped on panic too.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        disable_terminal_features();
        previous(info);
    }));
}

fn disable_terminal_features() {
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, PopKeyboardEnhancementFlags, DisableMouseCapture);
}

struct EventLoop {
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    app: App,
    renderer: Arc<RendererHandle>,
    popup: Arc<Popup>,
    selector: Arc<ModelSelector>,
    config_service: Option<Arc<ConfigService>>,
    tx: mpsc::Sender<AppMsg>,
    rx: mpsc::Receiver<AppMsg>,
}

impl EventLoop {
    #[allow(clippy::too_many_arguments)]
    fn new(
        agent_loop: Arc<AgentLoop>,
        agent_id: String,
        session_id: String,
        renderer: Arc<RendererHandle>,
        input: Arc<Input>,
        popup: Arc<Popup>,
        selector: Arc<ModelSelector>,
        config_service: Option<Arc<ConfigService>>,
    ) -> Self {
        // Bound generous enough to absorb bursts of stream events; the
        // input task bails out if the loop ever stops draining.
        let (tx, rx) = mpsc::channel(256);
        input.spawn(tx.clone());
        EventLoop {
            agent_loop,
            agent_id,
            session_id,
            app: App::new(),
            renderer,
            popup,
            selector,
            config_service,
            tx,
            rx,
        }
    }

    async fn run(
        mut self,
        mut terminal: ratatui::DefaultTerminal,
    ) -> std::result::Result<(), String> {
        loop {
            let snapshot = self.popup.snapshot();
            terminal
                .draw(|f| {
                    view::draw_with_popup(
                        f,
                        &mut self.app,
                        self.renderer.as_renderer(),
                        snapshot.as_ref(),
                    )
                })
                .map_err(|e| e.to_string())?;

            let msg = tokio::select! {
                m = self.rx.recv() => match m {
                    Some(msg) => msg,
                    None => break,
                },
                _ = tokio::time::sleep(RENDER_TICK) => continue,
            };

            // Popup-first key routing: while open, arrows/Enter/Esc belong
            // to the popup and never reach the editor; `/model` Enter opens
            // it instead of submitting as chat.
            if let AppMsg::Key(key) = &msg {
                if self.popup.is_open() {
                    if self.handle_popup_key(*key) {
                        if self.app.should_quit() {
                            break;
                        }
                        continue;
                    }
                } else if *key == KeyEvent::Enter && self.app.input().trim() == "/model" {
                    self.open_model_popup();
                    continue;
                }
            }

            let effect = self.app.reduce(msg);
            if let Some(input) = effect.submitted {
                self.spawn_turn(input);
            }
            if self.app.should_quit() {
                break;
            }
        }
        Ok(())
    }

    /// Handles one key while the model popup is open. Returns `true` when
    /// the key belonged to the popup (consumed; never reaches `App`).
    /// `Up`/`Down` move the cursor, `Esc` dismisses, `Enter` applies the
    /// highlighted model (persisted to `config.toml` when a file-backed
    /// config service is present, rolled back otherwise) and resumes
    /// normal input. Anything else is swallowed so typing can't leak into
    /// the cleared editor mid-select.
    fn handle_popup_key(&mut self, key: KeyEvent) -> bool {
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
                    let notice = apply_model_selection(
                        &self.selector,
                        self.config_service.as_deref(),
                        &model,
                    );
                    self.app.update(AppMsg::Notice(notice));
                }
                self.popup.close();
                true
            }
            KeyEvent::Interrupt => false,
            _ => true,
        }
    }

    /// Opens the model popup for an exact `/model` Enter: clears the
    /// command line, shows the cached catalog instantly, then refreshes
    /// `GET {base_url}/models` in the background without blocking input.
    fn open_model_popup(&mut self) {
        let current = self.selector.current();
        let models = self.selector.models();
        let selected = models.iter().position(|m| *m == current).unwrap_or(0);
        self.app.clear_input();
        self.popup.open(" model ", models, selected, Some(current));
        let selector = self.selector.clone();
        let popup = self.popup.clone();
        tokio::spawn(async move {
            if let Some(ids) = selector.refresh().await {
                popup.refresh_items(ids, Some(selector.current()));
            }
        });
    }

    /// Spawns a task that runs one agent turn and forwards its stream
    /// into the app channel. Fully concurrent: submitting again while a
    /// turn streams simply starts another one.
    fn spawn_turn(&mut self, input: String) {
        self.app.turn_started();
        let agent_loop = self.agent_loop.clone();
        let agent_id = self.agent_id.clone();
        let session_id = self.session_id.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut rx = agent_loop.run(&agent_id, &session_id, input);
            while let Some(event) = rx.recv().await {
                let msg = match event {
                    // Lifecycle bookkeeping the chat pane doesn't render.
                    TurnEvent::Started | TurnEvent::Iteration(_) => continue,
                    TurnEvent::Assistant(text) => AppMsg::Assistant(text),
                    TurnEvent::ToolStarted(call) => AppMsg::ToolStarted(call),
                    TurnEvent::ToolResult(call, result) => AppMsg::ToolResult(call, result),
                    TurnEvent::Completed(n) => AppMsg::Completed(n),
                    TurnEvent::Failed(err) => AppMsg::Failed(err),
                };
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
            // The stream ended: release the busy slot.
            let _ = tx.send(AppMsg::TurnFinished).await;
        });
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
fn apply_model_selection(
    selector: &ModelSelector,
    service: Option<&ConfigService>,
    model: &str,
) -> String {
    let (prev_current, prev_catalog) = selector.snapshot();
    selector.set_current(model);
    let Some(service) = service else {
        return format!("model → {model} (session only)");
    };
    match service.set_llm_model(model) {
        Ok(_) => {
            selector.sync_default(model);
            format!("model → {model} (saved)")
        }
        Err(err) => {
            selector.restore(prev_current.clone(), prev_catalog);
            format!("✗ config save failed: {err} (kept {prev_current})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_config::{AppConfig, ConfigService};

    fn test_config(model: &str) -> AppConfig {
        AppConfig::from_toml(
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

        let notice = apply_model_selection(&selector, Some(&service), "m2");

        assert_eq!(notice, "model → m2 (saved)");
        assert_eq!(selector.current(), "m2");
        assert_eq!(selector.default_model(), "m2");
        assert_eq!(service.get().llm.model, "m2");
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

        let notice = apply_model_selection(&selector, Some(&service), "m2");

        assert!(notice.contains("kept m1"), "{notice:?}");
        assert_eq!(
            selector.snapshot(),
            ("m1".to_owned(), vec!["m1".to_owned()])
        );
        assert_eq!(service.get().llm.model, "m1");
    }

    #[test]
    fn no_service_degrades_to_session_only() {
        let selector = test_selector("m1");

        let notice = apply_model_selection(&selector, None, "m2");

        assert_eq!(notice, "model → m2 (session only)");
        assert_eq!(selector.current(), "m2");
    }
}
