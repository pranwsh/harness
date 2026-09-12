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

use harness_agent_loop::{AgentLoop, TurnEvent};
use harness_tui_commands::CommandPopup;
use harness_tui_input::Input;
use harness_tui_model::ModelPopup;
use harness_tui_state::{
    app::{App, AppMsg, KeyEvent},
    render::RendererHandle,
};

use crate::view::{self, LayoutCache};

/// How often to redraw even without input, so the busy hint and
/// late-arriving stream events are always fresh.
const RENDER_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Runs the TUI until the app quits. Restores the terminal on exit,
/// including on panic (hooks below). Services come from DI so alternative
/// renderers or input sources plug in without touching this loop.
///
/// Each provider owns its own popup surface and all domain behavior.
/// The slash-commands provider (`command_popup`) filters `/`-prefixes
/// non-modally (cursor stays in the entry bar); the model provider
/// (`model_popup`) owns the modal `/model` selector. The loop draws at
/// most one snapshot (model first, else commands) and itself stays
/// domain-free, never touching catalogs or config directly.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    renderer: Arc<RendererHandle>,
    input: Arc<Input>,
    model_popup: Arc<ModelPopup>,
    command_popup: Arc<CommandPopup>,
) {
    let terminal = ratatui::init();
    enable_terminal_features();
    let result = EventLoop::new(
        agent_loop,
        agent_id,
        session_id,
        renderer,
        input,
        model_popup,
        command_popup,
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
    model_popup: Arc<ModelPopup>,
    command_popup: Arc<CommandPopup>,
    tx: mpsc::Sender<AppMsg>,
    rx: mpsc::Receiver<AppMsg>,
    layout_cache: LayoutCache,
}

impl EventLoop {
    #[allow(clippy::too_many_arguments)]
    fn new(
        agent_loop: Arc<AgentLoop>,
        agent_id: String,
        session_id: String,
        renderer: Arc<RendererHandle>,
        input: Arc<Input>,
        model_popup: Arc<ModelPopup>,
        command_popup: Arc<CommandPopup>,
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
            model_popup,
            command_popup,
            tx,
            rx,
            layout_cache: LayoutCache::new(),
        }
    }

    async fn run(
        mut self,
        mut terminal: ratatui::DefaultTerminal,
    ) -> std::result::Result<(), String> {
        loop {
            // Each provider owns its surface; draw at most one, model
            // first so the modal selector wins over autocomplete.
            let snapshot = self
                .model_popup
                .snapshot()
                .or_else(|| self.command_popup.snapshot());
            terminal
                .draw(|f| {
                    view::draw_with_popup_cached(
                        f,
                        &mut self.app,
                        self.renderer.as_renderer(),
                        snapshot.as_ref(),
                        &mut self.layout_cache,
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

            // Non-modal filter popups, each provider owning its decisions
            // and its surface; this loop stays domain-free:
            // - Model search active: nav/completion keys belong to it;
            //   editing keys fall through so the entry bar stays live, then
            //   `sync` re-filters the catalog.
            // - Else slash open + nav/completion key: Up/Down/Esc/Tab move,
            //   dismiss, or complete via `set_input`. Exact-`Enter` falls
            //   through to execute.
            // - Else exact `/model` Enter: opens model search and closes
            //   the slash list (the staged `/model ` line still matches a
            //   slash candidate, so it cannot self-close).
            // - Else: normal edit; post-reduce syncs the eligible provider.
            if let AppMsg::Key(key) = &msg {
                if self.model_popup.is_active() {
                    if self.model_popup.handle_key(*key, &mut self.app) {
                        if self.app.should_quit() {
                            break;
                        }
                        continue;
                    }
                } else {
                    if self.command_popup.is_open()
                        && matches!(
                            *key,
                            KeyEvent::Up
                                | KeyEvent::Down
                                | KeyEvent::Esc
                                | KeyEvent::Tab
                                | KeyEvent::Enter
                        )
                        && self.command_popup.handle_key(*key, &mut self.app)
                    {
                        if self.app.should_quit() {
                            break;
                        }
                        continue;
                    }
                    if *key == KeyEvent::Enter && self.model_popup.wants_input(self.app.input()) {
                        self.model_popup.open(&mut self.app);
                        self.command_popup.close();
                        continue;
                    }
                }
            }

            let is_key = matches!(msg, AppMsg::Key(_));
            let effect = self.app.reduce(msg);
            if is_key {
                // Only the eligible provider re-filters: an active search
                // suppresses slash completion, and a session ended by
                // editing (e.g. backspaced to `/mode`) falls straight
                // through to slash on the same keystroke.
                if self.model_popup.is_active() {
                    self.model_popup.sync(&self.app);
                }
                if !self.model_popup.is_active() {
                    self.command_popup.sync(&self.app);
                }
            }
            if let Some(input) = effect.submitted {
                self.spawn_turn(input);
            }
            if self.app.should_quit() {
                break;
            }
        }
        Ok(())
    }

    /// Spawns a task that runs one agent turn and forwards its stream
    /// into the app channel. Reject-while-busy: if a turn is already
    /// streaming, new submits are rejected in [`App::submit`] with an
    /// inline error and never reach here, so turns never interleave on
    /// one session.
    fn spawn_turn(&mut self, input: String) {
        if self.app.is_busy() {
            // Defense-in-depth: `App::submit` already rejected, but release
            // builds skip `debug_assert` — fail closed rather than
            // interleaving two turns on one session.
            return;
        }
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
                    TurnEvent::AssistantDelta(delta) => AppMsg::AssistantDelta(delta),
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
