//! TUI front-end: a ratatui-based chat interface over the agent loop.
//!
//! Thin shell crate: terminal I/O and DI wiring only. Pure state lives in
//! [`harness_tui_state`], markdown rendering in [`harness_tui_markdown`].
//!
//! Layout of concerns:
//! - [`input`]: crossterm key/mouse reading task.
//! - [`view`]: ratatui rendering, a pure function of `App` plus a renderer.
//! - [`runtime`]: terminal lifecycle and the main event loop.
//! - [`Tui`]/[`TuiPlugin`]: DI facade and plugin wiring.

use std::sync::Arc;

use harness_contracts::{KEY_AGENT_LOOP, KEY_INPUT, KEY_MARKDOWN_RENDERER, KEY_TUI};
use harness_core::{Context, Result};
use tokio::sync::mpsc;

use harness_agent_loop::AgentLoop;
use harness_tui_input::Input;
use harness_tui_state::render::RendererHandle;

pub mod runtime;
pub mod view;

/// Chat front-end bound to one agent and session. Running it takes over
/// the terminal until the user quits via `/quit` or Ctrl+C. Rendering and
/// input come from injected DI services so engines stay swappable.
pub struct Tui {
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    renderer: Arc<RendererHandle>,
    input: Arc<Input>,
    done: mpsc::Sender<()>,
}

impl Tui {
    pub fn new(
        agent_loop: Arc<AgentLoop>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        renderer: Arc<RendererHandle>,
        input: Arc<Input>,
        done: mpsc::Sender<()>,
    ) -> Self {
        Tui {
            agent_loop,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            renderer,
            input,
            done,
        }
    }

    /// Runs until `/quit`, Ctrl+C, or a terminal failure. On return the
    /// terminal has been restored and `done` has been signalled.
    pub async fn run(&self) {
        runtime::run(
            self.agent_loop.clone(),
            self.agent_id.clone(),
            self.session_id.clone(),
            self.renderer.clone(),
            self.input.clone(),
        )
        .await;
        let _ = self.done.send(()).await;
    }
}

pub struct TuiPlugin {
    done: mpsc::Sender<()>,
}

impl TuiPlugin {
    pub fn new(done: mpsc::Sender<()>) -> Self {
        TuiPlugin { done }
    }
}

impl harness_core::Plugin for TuiPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui")
            .provides(KEY_TUI)
            .injects(KEY_AGENT_LOOP)
            .injects(KEY_MARKDOWN_RENDERER)
            .injects(KEY_INPUT)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let agent_loop: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP)?;
        let renderer: Arc<RendererHandle> = ctx.inject_key(KEY_MARKDOWN_RENDERER)?;
        let input: Arc<Input> = ctx.inject_key(KEY_INPUT)?;
        ctx.provide_key(
            KEY_TUI,
            Arc::new(Tui::new(
                agent_loop,
                "agent-1",
                "session-1",
                renderer,
                input,
                self.done.clone(),
            )),
        );
        Ok(())
    }
}
