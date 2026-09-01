use std::sync::Arc;

use harness_contracts::{KEY_AGENT_LOOP, KEY_REPL, KEY_SESSIONS};
use harness_core::{Context, Result};
use tokio::sync::mpsc;

use harness_agent_loop::{AgentLoop, TurnEvent};
use harness_session::SessionLog;

/// Line-oriented REPL. Consumes the agent loop's turn stream and renders it
/// to stdout; subscribes to nothing, emits nothing.
pub struct Repl {
    ctx: Context,
    agent_loop: Arc<AgentLoop>,
    agent_id: String,
    session_id: String,
    done: mpsc::Sender<()>,
}

impl Repl {
    pub fn new(
        ctx: Context,
        agent_loop: Arc<AgentLoop>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        done: mpsc::Sender<()>,
    ) -> Self {
        Repl {
            ctx,
            agent_loop,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            done,
        }
    }

    /// Runs until `/quit` or EOF. Each turn's stream is rendered as it arrives.
    pub async fn run(&self) {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        loop {
            print!("> ");
            std::io::Write::flush(&mut std::io::stdout()).ok();
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                _ => break,
            };
            let input = line.trim().to_owned();
            if input.is_empty() {
                continue;
            }
            match input.as_str() {
                "/quit" | "/exit" => break,
                "/history" => {
                    self.print_history();
                    continue;
                }
                cmd if cmd.starts_with('/') => {
                    println!("unknown command: {cmd}");
                    continue;
                }
                _ => {}
            }
            self.turn(&input).await;
        }
        let _ = self.done.send(()).await;
    }

    async fn turn(&self, input: &str) {
        let mut rx = self.agent_loop.run(&self.agent_id, &self.session_id, input);
        while let Some(event) = rx.recv().await {
            self.render(event);
        }
    }

    fn print_history(&self) {
        let Some(sessions) = self.ctx.try_inject_key::<Arc<SessionLog>>(KEY_SESSIONS) else {
            return;
        };
        for (i, entry) in sessions.history(&self.session_id).iter().enumerate() {
            println!("{:>3} [{:?}] {}", i, entry.role, entry.content);
        }
    }

    fn render(&self, event: TurnEvent) {
        match event {
            TurnEvent::Started => {}
            TurnEvent::Iteration(n) => println!("── iteration {n} ──"),
            TurnEvent::Assistant(text) => println!("{text}"),
            TurnEvent::ToolStarted(call) => {
                println!("→ tool {} ({})", call.name, call.arguments);
            }
            TurnEvent::ToolResult(call, Ok(result)) => {
                println!("← {} ok: {}", call.name, truncate(&result, 200));
            }
            TurnEvent::ToolResult(call, Err(err)) => {
                println!("← {} failed: {err}", call.name);
            }
            TurnEvent::Completed(n) => {
                println!("· done ({n} iteration{})", if n == 1 { "" } else { "s" });
            }
            TurnEvent::Failed(err) => println!("✗ {err}"),
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub struct ReplPlugin {
    done: mpsc::Sender<()>,
}

impl ReplPlugin {
    pub fn new(done: mpsc::Sender<()>) -> Self {
        ReplPlugin { done }
    }
}

impl harness_core::Plugin for ReplPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("repl")
            .provides(KEY_REPL)
            .injects(KEY_AGENT_LOOP)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let agent_loop: Arc<AgentLoop> = ctx.inject_key(KEY_AGENT_LOOP)?;
        ctx.provide_key(
            KEY_REPL,
            Arc::new(Repl::new(
                ctx.clone(),
                agent_loop,
                "agent-1",
                "session-1",
                self.done.clone(),
            )),
        );
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_cuts_long_strings() {
        assert_eq!(truncate("hello world", 5), "hello");
        assert_eq!(truncate("hi", 5), "hi");
    }
}
