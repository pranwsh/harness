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
use harness_contracts::SessionStoreHandle;
use harness_tui_commands::CommandPopup;
use harness_tui_input::Input;
use harness_tui_model::ModelPopup;
use harness_tui_sessions::SessionPopup;
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
/// (`model_popup`) owns the modal `/model` selector; the sessions provider
/// (`session_popup`) owns the `/sessions` picker. The loop draws at most
/// one snapshot (model first, then sessions, else commands) and itself
/// stays domain-free, never touching catalogs or config directly.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    sessions: Arc<SessionStoreHandle>,
    renderer: Arc<RendererHandle>,
    input: Arc<Input>,
    model_popup: Arc<ModelPopup>,
    command_popup: Arc<CommandPopup>,
    session_popup: Arc<SessionPopup>,
) {
    let terminal = ratatui::init();
    enable_terminal_features();
    let result = EventLoop::new(
        agent_loop,
        agent_id,
        session_id,
        sessions,
        renderer,
        input,
        model_popup,
        command_popup,
        session_popup,
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
    sessions: Arc<SessionStoreHandle>,
    app: App,
    renderer: Arc<RendererHandle>,
    model_popup: Arc<ModelPopup>,
    command_popup: Arc<CommandPopup>,
    session_popup: Arc<SessionPopup>,
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
        sessions: Arc<SessionStoreHandle>,
        renderer: Arc<RendererHandle>,
        input: Arc<Input>,
        model_popup: Arc<ModelPopup>,
        command_popup: Arc<CommandPopup>,
        session_popup: Arc<SessionPopup>,
    ) -> Self {
        // Bound generous enough to absorb bursts of stream events; the
        // input task bails out if the loop ever stops draining.
        let (tx, rx) = mpsc::channel(256);
        input.spawn(tx.clone());
        session_popup.set_current(&session_id);
        EventLoop {
            agent_loop,
            agent_id,
            session_id,
            sessions,
            app: App::new(),
            renderer,
            model_popup,
            command_popup,
            session_popup,
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
            // first, then sessions, so the modal selectors win over
            // autocomplete.
            let snapshot = self
                .model_popup
                .snapshot()
                .or_else(|| self.session_popup.snapshot())
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
            // - Else sessions picker active: same contract, keys belong to
            //   it while live.
            // - Else slash open + nav/completion key: Up/Down/Esc/Tab move,
            //   dismiss, or complete via `set_input`. Exact-`Enter` falls
            //   through to execute.
            // - Else exact `/model` Enter: opens model search (and exact
            //   `/sessions` Enter opens the picker), closing the slash list
            //   (the staged `/model ` / `/sessions ` lines still match a
            //   slash candidate, so they cannot self-close).
            // - Else: normal edit; post-reduce syncs the eligible provider.
            if let AppMsg::Key(key) = &msg {
                if self.model_popup.is_active() {
                    if self.model_popup.handle_key(*key, &mut self.app) {
                        if self.app.should_quit() {
                            break;
                        }
                        self.drain_switch_request();
                        continue;
                    }
                } else if self.session_popup.is_active() {
                    if self.session_popup.handle_key(*key, &mut self.app) {
                        if self.app.should_quit() {
                            break;
                        }
                        self.drain_switch_request();
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
                    if *key == KeyEvent::Enter
                        && self.session_popup.wants_input(self.app.input())
                    {
                        self.session_popup.open(&mut self.app);
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
                if self.session_popup.is_active() {
                    self.session_popup.sync(&self.app);
                }
                if !self.model_popup.is_active() && !self.session_popup.is_active() {
                    self.command_popup.sync(&self.app);
                }
                self.drain_switch_request();
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

    /// Drains a staged `/sessions` resume pick, if any. Reject-while-busy
    /// (mirroring submit): a switch mid-turn would interleave two turns on
    /// different sessions, so the pick is dropped with an inline error and
    /// the user re-picks after the turn finishes.
    fn drain_switch_request(&mut self) {
        let Some(session_id) = self.session_popup.take_switch_request() else {
            return;
        };
        if self.app.is_busy() {
            self.app.update(AppMsg::Notice(
                "busy: wait for the current turn to finish before switching sessions".into(),
            ));
            return;
        }
        self.switch_session(session_id);
    }

    /// Swaps the live session: reloads the transcript from the session
    /// store and retargets future turns. History comes through the
    /// decoupled store handle (lazily hydrated from disk by the session
    /// plugin); rendering the entries is pure `App` state.
    fn switch_session(&mut self, session_id: String) {
        let history = self.sessions.history(&session_id);
        let entries = history.len();
        self.app.set_transcript(&history);
        self.session_id = session_id.clone();
        self.session_popup.set_current(&session_id);
        let short: String = session_id.chars().take(8).collect();
        self.app.update(AppMsg::Notice(format!(
            "resumed session {short} ({entries} {})",
            if entries == 1 { "entry" } else { "entries" }
        )));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use harness_contracts::{
        AgentRegistryApi, AgentRegistryHandle, AgentState, BoxFuture, Entry, Message,
        ModelCatalogApi, ModelCatalogHandle, ModelSelectorApi, ModelSelectorHandle,
        ModelStreamerApi, ModelStreamerHandle, PromptAssemblerApi, PromptAssemblerHandle,
        SessionCatalogApi, SessionCatalogHandle, SessionStoreApi, SessionSummary, StreamEvent,
        ToolExecutorApi, ToolExecutorHandle, ToolSpec,
    };
    use harness_core::Context;
    use harness_session::SessionLog;
    use harness_tui_state::app::ItemKind;
    use harness_tui_state::render::{ASSISTANT_BASE, PlainRenderer};

    struct Registry;

    impl AgentRegistryApi for Registry {
        fn get_or_create(&self, _id: &str) {}
        fn set_state(&self, _id: &str, _state: AgentState) {}
        fn state_of(&self, _id: &str) -> Option<AgentState> {
            None
        }
    }

    struct Tools;

    impl ToolExecutorApi for Tools {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        fn execute(
            &self,
            _agent_id: &str,
            _session_id: &str,
            _turn: u64,
            _call: harness_contracts::ToolCall,
        ) -> BoxFuture<Result<String, harness_contracts::ToolError>> {
            Box::pin(async move { Ok(String::new()) })
        }
    }

    struct Prompt;

    impl PromptAssemblerApi for Prompt {
        fn assemble(
            &self,
            _agent_id: &str,
            _session_id: &str,
            _iteration: u32,
            _system_prompt: &str,
            _history: &[Entry],
        ) -> Vec<Message> {
            Vec::new()
        }
    }

    struct Selector;

    impl ModelSelectorApi for Selector {
        fn select(&self, _agent_id: &str) -> String {
            "m".into()
        }
    }

    struct Streamer;

    impl ModelStreamerApi for Streamer {
        fn stream(
            self: Arc<Self>,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
        ) -> tokio::sync::mpsc::Receiver<StreamEvent> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            rx
        }
    }

    /// One fake serving both the model popup (needs the rich catalog) and
    /// the session picker (needs the session catalog): the providers only
    /// ever see their own trait, which is exactly the decoupling under
    /// test.
    struct Catalog {
        models: Vec<String>,
        sessions: Mutex<Vec<SessionSummary>>,
    }

    impl ModelCatalogApi for Catalog {
        fn current(&self) -> String {
            self.models.first().cloned().unwrap_or_default()
        }
        fn default_model(&self) -> String {
            self.current()
        }
        fn models(&self) -> Vec<String> {
            self.models.clone()
        }
        fn set_current(&self, _model: &str) {}
        fn set_catalog(&self, _models: Vec<String>) {}
        fn snapshot(&self) -> (String, Vec<String>) {
            (self.current(), self.models())
        }
        fn restore(&self, _current: String, _catalog: Vec<String>) {}
        fn sync_default(&self, _model: &str) {}
        fn refresh(self: Arc<Self>) -> BoxFuture<Option<Vec<String>>> {
            Box::pin(async move { None })
        }
    }

    impl SessionCatalogApi for Catalog {
        fn list(&self) -> Vec<SessionSummary> {
            self.summaries()
        }
    }

    impl Catalog {
        fn summaries(&self) -> Vec<SessionSummary> {
            self.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    fn summary(id: &str, title: &str) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            title: title.into(),
            created_at: 1000,
            updated_at: 2000,
            turns: 1,
            entries: 2,
        }
    }

    struct Fixture {
        events: EventLoop,
    }

    /// Builds the loop with a real (in-memory) session store pre-seeded
    /// with two entries on `"past-1"`, and a picker catalog listing it.
    fn fixture() -> (Arc<SessionLog>, Fixture) {
        let ctx = Context::root();
        let log = Arc::new(SessionLog::new(ctx.clone()));
        let turn = log.begin_turn("past-1");
        log.append("past-1", turn, Entry::from_message(&Message::user("old question")));
        log.append(
            "past-1",
            turn,
            Entry::from_message(&Message::assistant("old answer")),
        );
        let store = Arc::new(SessionStoreHandle(log.clone() as Arc<dyn SessionStoreApi>));

        let agent_loop = Arc::new(AgentLoop::new(
            ctx,
            Arc::new(AgentRegistryHandle(Arc::new(Registry))),
            store.clone(),
            Arc::new(ToolExecutorHandle(Arc::new(Tools))),
            Arc::new(PromptAssemblerHandle(Arc::new(Prompt))),
            Arc::new(ModelSelectorHandle(Arc::new(Selector))),
            Arc::new(ModelStreamerHandle(Arc::new(Streamer))),
        ));
        let catalog = Arc::new(Catalog {
            models: vec!["m".into()],
            sessions: Mutex::new(vec![summary("past-1", "old chat")]),
        });
        let events = EventLoop::new(
            agent_loop,
            "agent-1".to_owned(),
            "live-1".to_owned(),
            store,
            Arc::new(RendererHandle(Arc::new(PlainRenderer::new(ASSISTANT_BASE)))),
            Arc::new(Input),
            Arc::new(ModelPopup::new(
                Arc::new(ModelCatalogHandle(catalog.clone() as Arc<dyn ModelCatalogApi>)),
                None,
            )),
            Arc::new(CommandPopup::new()),
            Arc::new(SessionPopup::new(Arc::new(SessionCatalogHandle(
                catalog as Arc<dyn SessionCatalogApi>,
            )))),
        );
        (log, Fixture { events })
    }

    #[tokio::test]
    async fn switch_session_reloads_transcript_and_retargets() {
        let (_log, mut fx) = fixture();
        fx.events.switch_session("past-1".to_owned());

        assert_eq!(fx.events.session_id, "past-1");
        let items = fx.events.app.items();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, ItemKind::User);
        assert_eq!(items[0].text, "old question");
        assert_eq!(items[1].kind, ItemKind::Assistant);
        assert_eq!(items[1].text, "old answer");
        assert_eq!(items[2].kind, ItemKind::Notice);
        assert!(
            items[2].text.starts_with("resumed session past-1 (2 entries)"),
            "got: {}",
            items[2].text
        );
        // View re-pins to the newest restored content.
        assert!(fx.events.app.follows());
    }

    #[tokio::test]
    async fn switch_to_unknown_session_starts_empty() {
        let (_log, mut fx) = fixture();
        fx.events.switch_session("never-seen".to_owned());

        assert_eq!(fx.events.session_id, "never-seen");
        let items = fx.events.app.items();
        assert_eq!(items.len(), 1);
        assert!(items[0].text.contains("(0 entries)"), "got: {}", items[0].text);
    }

    #[tokio::test]
    async fn switch_while_busy_is_dropped_with_notice() {
        let (_log, mut fx) = fixture();

        // Stage a real picker pick: open on a scratch app, complete the
        // single row, exact-Enter to stage the resume.
        let mut scratch = App::new();
        fx.events.session_popup.open(&mut scratch);
        assert!(fx.events.session_popup.handle_key(KeyEvent::Enter, &mut scratch));
        assert!(fx.events.session_popup.handle_key(KeyEvent::Enter, &mut scratch));

        fx.events.app.turn_started();
        fx.events.drain_switch_request();

        // Dropped, not queued: still on the live session with an inline
        // error, and the staged pick is consumed.
        assert_eq!(fx.events.session_id, "live-1");
        let items = fx.events.app.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Notice);
        assert!(items[0].text.contains("busy"), "got: {}", items[0].text);
        assert!(fx.events.session_popup.take_switch_request().is_none());

        // …so after the turn finishes, nothing switches on its own: the
        // user re-picks.
        fx.events.app.turn_finished();
        fx.events.drain_switch_request();
        assert_eq!(fx.events.session_id, "live-1");
        assert_eq!(fx.events.app.items().len(), 1);
    }

    #[tokio::test]
    async fn drain_without_pick_is_noop() {
        let (_log, mut fx) = fixture();
        fx.events.app.turn_started();
        fx.events.drain_switch_request();
        assert_eq!(fx.events.session_id, "live-1");
        assert!(fx.events.app.items().is_empty());
        fx.events.app.turn_finished();
    }
}
