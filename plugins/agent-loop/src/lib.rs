use std::sync::Arc;

use harness_contracts::{
    AgentRegistryHandle, AgentState, CH_SESSION_APPEND_REQUESTED, CH_SESSION_TURN_CLOSE_REQUESTED,
    CH_SESSION_TURN_OPEN_REQUESTED, CH_TURN_COMPLETED, CH_TURN_FAILED, CH_TURN_ITERATION,
    CH_TURN_STARTED, ConfigHandle, Entry, KEY_AGENT_LOOP, KEY_AGENTS_API, KEY_CONFIG,
    KEY_MODEL_SELECTOR_API, KEY_MODEL_STREAMER, KEY_PROMPT_ASSEMBLER, KEY_PROMPT_TEXT,
    KEY_SESSION_STORE, KEY_TOOL_EXECUTOR, Message, ModelSelectorHandle, ModelStreamerHandle,
    PromptAssemblerHandle, SessionAppendRequested, SessionId, SessionStoreHandle,
    SessionTurnCloseRequested, SessionTurnOpenRequested, StreamEvent, ToolCall, ToolExecutorHandle,
    TurnCompleted, TurnFailed, TurnIteration, TurnStarted,
};
use harness_core::{Context, Result};
use tokio::sync::mpsc;

/// Events surfaced to UI consumers on the per-turn stream.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    Started,
    Iteration(u32),
    /// A slice of assistant text, to display immediately. Slices for one
    /// reply arrive in order; the loop stores the full message once it
    /// completes and never re-emits it whole.
    AssistantDelta(String),
    ToolStarted(ToolCall),
    ToolResult(ToolCall, Result<String, harness_contracts::ToolError>),
    Completed(u32),
    Failed(String),
}

/// The orchestrator. One `run` spawns a turn task and returns a stream the
/// caller (e.g. the REPL) consumes for rendering.
///
/// This crate depends only on `harness-contracts` and `harness-core` —
/// never on a sibling provider crate. All collaborators arrive as sized
/// trait handles (`Arc<*Handle>`) injected under the `*_API` / store keys.
///
/// Prompt text (`prompt.text`) and loop bounds (`agent.limits`) are pulled
/// fresh from the context on every turn/iteration — never snapshotted at
/// startup — so live config or prompt updates apply to the next iteration.
///
/// Persistence is owned by the session plugin: all mutations (turn
/// open/close, user/assistant appends) go through the bus
/// (`session.turn_open_requested` waterfall + `session.append_requested` /
/// `session.turn_close_requested` sync listeners); tool results travel via
/// `tool.executed`. Direct `SessionLog` writes are forbidden; the read
/// path (`history`) stays as a direct handle call.
///
/// UI delivery (`TurnEvent` mpsc stream) and bus broadcast (`turn.*`
/// channels) are intentionally dual: the stream is backpressure-aware for
/// the DI-shell TUI, the bus is telemetry for other listeners.
///
/// All continuation decisions are awaited service calls — events emitted on
/// the bus are pure notifications and never drive control flow, so listener
/// registration order can never affect correctness.
pub struct AgentLoop {
    ctx: Context,
    agents: Arc<AgentRegistryHandle>,
    sessions: Arc<SessionStoreHandle>,
    tools: Arc<ToolExecutorHandle>,
    prompt: Arc<PromptAssemblerHandle>,
    selector: Arc<ModelSelectorHandle>,
    model: Arc<ModelStreamerHandle>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: Context,
        agents: Arc<AgentRegistryHandle>,
        sessions: Arc<SessionStoreHandle>,
        tools: Arc<ToolExecutorHandle>,
        prompt: Arc<PromptAssemblerHandle>,
        selector: Arc<ModelSelectorHandle>,
        model: Arc<ModelStreamerHandle>,
    ) -> Self {
        AgentLoop {
            ctx,
            agents,
            sessions,
            tools,
            prompt,
            selector,
            model,
        }
    }

    /// Runs one turn: spawns the orchestration task and returns the event
    /// stream. The stream ends when the turn completes or fails.
    pub fn run(
        &self,
        agent_id: &str,
        session_id: &str,
        input: impl Into<String>,
    ) -> mpsc::Receiver<TurnEvent> {
        let (tx, rx) = mpsc::channel(64);
        let task = TurnTask {
            ctx: self.ctx.clone(),
            agents: self.agents.clone(),
            sessions: self.sessions.clone(),
            tools: self.tools.clone(),
            prompt: self.prompt.clone(),
            selector: self.selector.clone(),
            model: self.model.clone(),
            agent_id: agent_id.to_owned(),
            session_id: session_id.to_owned(),
            input: input.into(),
            tx,
            turn_no: 0,
        };
        tokio::spawn(async move {
            task.execute().await;
        });
        rx
    }
}

struct TurnTask {
    ctx: Context,
    agents: Arc<AgentRegistryHandle>,
    sessions: Arc<SessionStoreHandle>,
    tools: Arc<ToolExecutorHandle>,
    prompt: Arc<PromptAssemblerHandle>,
    selector: Arc<ModelSelectorHandle>,
    model: Arc<ModelStreamerHandle>,
    agent_id: String,
    session_id: SessionId,
    input: String,
    tx: mpsc::Sender<TurnEvent>,
    turn_no: u64,
}

impl TurnTask {
    /// Fresh prompt text for this iteration. Pulled live so a re-provided
    /// `prompt.text` applies to the next model call. Empty when the
    /// provider is absent (the assembler then omits the system message).
    fn system_prompt(&self) -> String {
        self.ctx
            .try_inject_key::<String>(KEY_PROMPT_TEXT)
            .map(|s| (*s).clone())
            .unwrap_or_default()
    }

    /// Fresh loop bound for this turn. Pulled live from the single
    /// `config.app` source of truth so live updates apply without rebuilding
    /// the loop. Falls back to the contract default.
    fn max_iterations(&self) -> u32 {
        self.ctx
            .try_inject_key::<ConfigHandle>(KEY_CONFIG)
            .map(|h| h.max_iterations())
            .unwrap_or(harness_contracts::default_max_iterations())
    }

    /// Canonical session write: emits `session.append_requested` and
    /// fail-closes when no session listener is present, so entries are
    /// never silently dropped (e.g. session plugin unloaded mid-run).
    fn request_append(&self, entry: Entry) -> std::result::Result<(), String> {
        let key: harness_core::Key = CH_SESSION_APPEND_REQUESTED.into();
        if self.ctx.listeners_of(&key).is_empty() {
            return Err("session persistence unavailable: no listener on session.append_requested".to_owned());
        }
        self.ctx
            .emit_key(
                CH_SESSION_APPEND_REQUESTED,
                SessionAppendRequested {
                    session_id: self.session_id.clone(),
                    turn: self.turn_no,
                    entry,
                },
            )
            .map(|_| ())
            .map_err(|e| format!("session append failed: {e}"))
    }

    /// Opens a new turn through the bus (waterfall). Fail-closed when no
    /// session handler is present so the loop never appends against a
    /// phantom turn.
    async fn request_open_turn(&self) -> std::result::Result<u64, String> {
        let req = SessionTurnOpenRequested {
            session_id: self.session_id.clone(),
            turn: 0,
        };
        let resp = self
            .ctx
            .waterfall_key(CH_SESSION_TURN_OPEN_REQUESTED, req)
            .await
            .map_err(|e| format!("session turn open failed: {e}"))?;
        if resp.turn == 0 {
            return Err("session turn open failed: no session handler".to_owned());
        }
        Ok(resp.turn)
    }

    /// Closes the current turn through the bus (one-way, no response).
    fn request_close_turn(&self) {
        let _ = self.ctx.emit_key(
            CH_SESSION_TURN_CLOSE_REQUESTED,
            SessionTurnCloseRequested {
                session_id: self.session_id.clone(),
                turn: self.turn_no,
            },
        );
    }
}

impl TurnTask {
    async fn execute(mut self) {
        let outcome = self.turn().await;
        match outcome {
            Ok(iterations) => {
                self.agents.set_state(&self.agent_id, AgentState::Idle);
                if self.turn_no != 0 {
                    self.request_close_turn();
                }
                let _ = self.tx.send(TurnEvent::Completed(iterations)).await;
                let _ = self.ctx.emit_key(
                    CH_TURN_COMPLETED,
                    TurnCompleted {
                        agent_id: self.agent_id.clone(),
                        session_id: self.session_id.clone(),
                        turn: self.turn_no,
                        iterations,
                    },
                );
            }
            Err(err) => {
                self.agents.set_state(&self.agent_id, AgentState::Failed);
                if self.turn_no != 0 {
                    self.request_close_turn();
                }
                let _ = self.tx.send(TurnEvent::Failed(err.clone())).await;
                let _ = self.ctx.emit_key(
                    CH_TURN_FAILED,
                    TurnFailed {
                        agent_id: self.agent_id.clone(),
                        session_id: self.session_id.clone(),
                        turn: self.turn_no,
                        error: err,
                    },
                );
            }
        }
    }

    async fn turn(&mut self) -> std::result::Result<u32, String> {
        // Ensure the agent exists, open the turn through the bus (the
        // session plugin is the sole owner of the monotonic counter), then
        // record the user input via the session plugin. Fail-closed when
        // the session handler is gone so input is never silently dropped.
        self.agents.get_or_create(&self.agent_id);
        self.agents.set_state(&self.agent_id, AgentState::Busy);
        self.turn_no = self.request_open_turn().await?;
        self.request_append(Entry::from_message(&Message::user(
            self.input.clone(),
        )))?;
        let _ = self.tx.send(TurnEvent::Started).await;
        let _ = self.ctx.emit_key(
            CH_TURN_STARTED,
            TurnStarted {
                agent_id: self.agent_id.clone(),
                session_id: self.session_id.clone(),
                turn: self.turn_no,
            },
        );

        let mut iterations = 0u32;
        loop {
            iterations += 1;
            let max_iterations = self.max_iterations();
            if iterations > max_iterations {
                return Err(format!(
                    "turn exceeded max iterations ({max_iterations})"
                ));
            }
            let _ = self.tx.send(TurnEvent::Iteration(iterations)).await;
            let _ = self.ctx.emit_key(
                CH_TURN_ITERATION,
                TurnIteration {
                    agent_id: self.agent_id.clone(),
                    session_id: self.session_id.clone(),
                    turn: self.turn_no,
                    iteration: iterations,
                },
            );

            // 1. Assemble prompt from current history. Prompt text is pulled
            // fresh every iteration so live updates apply to the next call.
            let history = self.sessions.history(&self.session_id);
            let system_prompt = self.system_prompt();
            let messages = self.prompt.assemble(
                &self.agent_id,
                &self.session_id,
                iterations,
                &system_prompt,
                &history,
            );

            // 2. Pick model, stream it. Text slices go out immediately;
            // the full message is stored once it completes.
            let model = self.selector.select(&self.agent_id);
            let mut stream =
                Arc::clone(&self.model.0).stream(&model, &messages, &self.tools.specs());
            let reply = loop {
                match stream.recv().await {
                    Some(StreamEvent::Content(delta)) => {
                        let _ = self.tx.send(TurnEvent::AssistantDelta(delta)).await;
                    }
                    Some(StreamEvent::Done(message)) => break message,
                    Some(StreamEvent::Failed(err)) => return Err(err),
                    // The producer task died without a terminal event:
                    // surface as a turn failure; shown text stays visible.
                    None => return Err("model stream ended without a response".to_owned()),
                }
            };

            // 3. Record the assistant message (deltas already displayed)
            // via the session plugin. Fail-closed on missing listener.
            self.request_append(Entry::from_message(&reply))?;

            // 4. No tool calls → done. Otherwise execute each call; the
            //    session plugin's sync listener has already appended the
            //    results by the time `execute` returns.
            if reply.tool_calls.is_empty() {
                return Ok(iterations);
            }
            for call in reply.tool_calls.clone() {
                let _ = self.tx.send(TurnEvent::ToolStarted(call.clone())).await;
                let result = self
                    .tools
                    .execute(&self.agent_id, &self.session_id, self.turn_no, call.clone())
                    .await;
                let _ = self.tx.send(TurnEvent::ToolResult(call, result)).await;
            }
        }
    }
}

pub struct AgentLoopPlugin;

impl harness_core::Plugin for AgentLoopPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("agent-loop")
            .provides(KEY_AGENT_LOOP)
            .injects(KEY_AGENTS_API)
            .injects(KEY_SESSION_STORE)
            .injects(KEY_TOOL_EXECUTOR)
            .injects(KEY_PROMPT_ASSEMBLER)
            .injects(KEY_PROMPT_TEXT)
            .injects(KEY_MODEL_SELECTOR_API)
            .injects(KEY_MODEL_STREAMER)
            .injects(KEY_CONFIG)
            .waterfalls::<SessionTurnOpenRequested>(CH_SESSION_TURN_OPEN_REQUESTED)
            .emits::<SessionTurnCloseRequested>(CH_SESSION_TURN_CLOSE_REQUESTED)
            .emits::<SessionAppendRequested>(CH_SESSION_APPEND_REQUESTED)
            .emits::<TurnStarted>(CH_TURN_STARTED)
            .emits::<TurnIteration>(CH_TURN_ITERATION)
            .emits::<TurnCompleted>(CH_TURN_COMPLETED)
            .emits::<TurnFailed>(CH_TURN_FAILED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        // `KEY_PROMPT_TEXT` / `KEY_CONFIG` stay declared as `injects`
        // for load-ordering, but their values are pulled per-turn (see
        // `TurnTask::system_prompt` / `max_iterations`) so live updates
        // apply without rebuilding the loop.
        let agents: Arc<AgentRegistryHandle> = ctx.inject_key(KEY_AGENTS_API)?;
        let sessions: Arc<SessionStoreHandle> = ctx.inject_key(KEY_SESSION_STORE)?;
        let tools: Arc<ToolExecutorHandle> = ctx.inject_key(KEY_TOOL_EXECUTOR)?;
        let prompt: Arc<PromptAssemblerHandle> = ctx.inject_key(KEY_PROMPT_ASSEMBLER)?;
        let _: Arc<String> = ctx.inject_key(KEY_PROMPT_TEXT)?;
        let selector: Arc<ModelSelectorHandle> = ctx.inject_key(KEY_MODEL_SELECTOR_API)?;
        let model: Arc<ModelStreamerHandle> = ctx.inject_key(KEY_MODEL_STREAMER)?;
        let _: Arc<ConfigHandle> = ctx.inject_key(KEY_CONFIG)?;

        let agent_loop = AgentLoop::new(ctx.clone(), agents, sessions, tools, prompt, selector, model);
        ctx.provide_key(KEY_AGENT_LOOP, Arc::new(agent_loop));
        Ok(())
    }
}
