//! TUI front-end: a ratatui-based chat interface over the agent loop.
//!
//! Layout of concerns:
//! - [`app`]: pure UI state (transcript, editor, scroll, quit).
//! - [`editor`]: multi-line input buffer with visual cursor motion.
//! - [`wrap`]: display-width-aware wrapping shared by editor and view.
//! - [`view`]: ratatui rendering, a pure function of `App`.
//! - [`input`]: crossterm key/mouse reading task.
//! - [`runtime`]: terminal lifecycle and the main event loop.
//! - [`Tui`]/[`TuiPlugin`]: DI facade and plugin wiring.

use std::sync::Arc;

use harness_contracts::{KEY_AGENT_LOOP, KEY_TUI};
use harness_core::{Context, Result};
use tokio::sync::mpsc;

use harness_agent_loop::AgentLoop;

pub mod app;
pub mod editor;
pub mod input;
pub mod runtime;
pub mod view;
pub mod wrap;

/// Chat front-end bound to one agent and session. Running it takes over
/// the terminal until the user quits via `/quit` or Ctrl+C.
pub struct Tui {
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    done: mpsc::Sender<()>,
}

impl Tui {
    pub fn new(
        agent_loop: Arc<AgentLoop>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        done: mpsc::Sender<()>,
    ) -> Self {
        Tui {
            agent_loop,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
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
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let agent_loop: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP)?;
        ctx.provide_key(
            KEY_TUI,
            Arc::new(Tui::new(
                agent_loop,
                "agent-1",
                "session-1",
                self.done.clone(),
            )),
        );
        Ok(())
    }
}
