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
use harness_tui_input::Input;
use harness_tui_model::ModelPopup;
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
///
/// The popup surface (`popup`) is the generic render state; the model
/// provider (`model_popup`) owns all `/model` behavior (trigger, key
/// routing, catalog refresh, persistence). The loop itself stays
/// domain-free and never touches the selector or config directly.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    renderer: Arc<RendererHandle>,
    input: Arc<Input>,
    popup: Arc<Popup>,
    model_popup: Arc<ModelPopup>,
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
        model_popup,
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
    model_popup: Arc<ModelPopup>,
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
        model_popup: Arc<ModelPopup>,
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
            model_popup,
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
            // to the popup provider and never reach the editor; a `/model`
            // Enter opens it instead of submitting as chat. Both decisions
            // live in the model plugin; this loop stays domain-free.
            if let AppMsg::Key(key) = &msg {
                if self.model_popup.is_open() {
                    if self.model_popup.handle_key(*key, &mut self.app) {
                        if self.app.should_quit() {
                            break;
                        }
                        continue;
                    }
                } else if *key == KeyEvent::Enter && self.model_popup.wants_input(self.app.input()) {
                    self.model_popup.open(&mut self.app);
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
