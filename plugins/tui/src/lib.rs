//! TUI front-end: a ratatui-based chat interface over the agent loop.
//!
//! Thin DI shell crate (documented exception to the `contracts`-only rule):
//! leaf UI crates may depend directly on provider crates (`harness-agent-loop`,
//! `harness-tui-state`, …); domain plugins must never depend on TUI. Pure
//! state lives in [`harness_tui_state`], markdown rendering in
//! [`harness_tui_markdown`]. The TUI observes the loop via its
//! `AgentLoop::run() -> Receiver<TurnEvent>` mpsc stream (backpressure-aware
//! delivery); the bus `turn.*` channels are telemetry-only.
//!
//! Layout of concerns:
//! - [`input`]: crossterm key/mouse reading task.
//! - [`view`]: ratatui rendering, a pure function of `App` plus a renderer.
//! - [`runtime`]: terminal lifecycle and the main event loop.
//! - [`Tui`]/[`TuiPlugin`]: DI facade and plugin wiring.

use std::sync::Arc;

use harness_contracts::{
    KEY_AGENT_LOOP, KEY_COMMAND_POPUP, KEY_INPUT, KEY_MARKDOWN_RENDERER, KEY_MODEL_POPUP,
    KEY_SESSION_POPUP, KEY_SESSION_STORE, KEY_TUI, SessionStoreHandle,
};
use harness_core::{Context, Result};
use tokio::sync::mpsc;

use harness_agent_loop::AgentLoop;
use harness_tui_commands::CommandPopup;
use harness_tui_input::Input;
use harness_tui_model::ModelPopup;
use harness_tui_sessions::SessionPopup;
use harness_tui_state::render::RendererHandle;

pub mod runtime;
pub mod view;

/// Chat front-end bound to one agent and session. Running it takes over
/// the terminal until the user quits via Ctrl+C. Rendering and
/// input come from injected DI services so engines stay swappable.
pub struct Tui {
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    sessions: Arc<SessionStoreHandle>,
    renderer: Arc<RendererHandle>,
    input: Arc<Input>,
    model_popup: Arc<ModelPopup>,
    command_popup: Arc<CommandPopup>,
    session_popup: Arc<SessionPopup>,
    done: mpsc::Sender<()>,
}

impl Tui {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_loop: Arc<AgentLoop>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        sessions: Arc<SessionStoreHandle>,
        renderer: Arc<RendererHandle>,
        input: Arc<Input>,
        model_popup: Arc<ModelPopup>,
        command_popup: Arc<CommandPopup>,
        session_popup: Arc<SessionPopup>,
        done: mpsc::Sender<()>,
    ) -> Self {
        Tui {
            agent_loop,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            sessions,
            renderer,
            input,
            model_popup,
            command_popup,
            session_popup,
            done,
        }
    }

    /// Runs until Ctrl+C or a terminal failure. On return the
    /// terminal has been restored and `done` has been signalled.
    pub async fn run(&self) {
        runtime::run(
            self.agent_loop.clone(),
            self.agent_id.clone(),
            self.session_id.clone(),
            self.sessions.clone(),
            self.renderer.clone(),
            self.input.clone(),
            self.model_popup.clone(),
            self.command_popup.clone(),
            self.session_popup.clone(),
        )
        .await;
        let _ = self.done.send(()).await;
    }
}

pub struct TuiPlugin {
    done: mpsc::Sender<()>,
    session_id: String,
}

impl TuiPlugin {
    pub fn new(done: mpsc::Sender<()>, session_id: impl Into<String>) -> Self {
        TuiPlugin {
            done,
            session_id: session_id.into(),
        }
    }
}

impl harness_core::Plugin for TuiPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui")
            .provides(KEY_TUI)
            .injects(KEY_AGENT_LOOP)
            .injects(KEY_SESSION_STORE)
            .injects(KEY_MARKDOWN_RENDERER)
            .injects(KEY_INPUT)
            .injects(KEY_MODEL_POPUP)
            .injects(KEY_COMMAND_POPUP)
            .injects(KEY_SESSION_POPUP)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let agent_loop: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP)?;
        let sessions: Arc<SessionStoreHandle> = ctx.inject_key(KEY_SESSION_STORE)?;
        let renderer: Arc<RendererHandle> = ctx.inject_key(KEY_MARKDOWN_RENDERER)?;
        let input: Arc<Input> = ctx.inject_key(KEY_INPUT)?;
        let model_popup: Arc<ModelPopup> = ctx.inject_key(KEY_MODEL_POPUP)?;
        let command_popup: Arc<CommandPopup> = ctx.inject_key(KEY_COMMAND_POPUP)?;
        let session_popup: Arc<SessionPopup> = ctx.inject_key(KEY_SESSION_POPUP)?;
        ctx.provide_key(
            KEY_TUI,
            Arc::new(Tui::new(
                agent_loop,
                "agent-1",
                self.session_id.clone(),
                sessions,
                renderer,
                input,
                model_popup,
                command_popup,
                session_popup,
                self.done.clone(),
            )),
        );
        Ok(())
    }
}
