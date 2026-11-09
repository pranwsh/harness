use std::sync::Arc;

use harness_contracts::{
    AgentRegistryHandle, AgentState, CH_SESSION_APPEND_REQUESTED, CH_SESSION_TURN_CLOSE_REQUESTED,
    CH_SESSION_TURN_OPEN_REQUESTED, CH_TOOL_EXECUTED, CH_TURN_COMPLETED, CH_TURN_FAILED,
    CH_TURN_ITERATION, CH_TURN_STARTED, ConfigHandle, Entry, KEY_AGENT_LOOP, KEY_AGENTS_API,
    KEY_CONFIG, KEY_MODEL_SELECTOR_API, KEY_MODEL_STREAMER, KEY_PROMPT_ASSEMBLER, KEY_PROMPT_TEXT,
    KEY_SESSION_STORE, KEY_TOOL_EXECUTOR, KEY_TOOL_REGISTRY, Message, ModelSelectorHandle,
    ModelStreamerHandle, PromptAssemblerHandle, Role, SessionAppendRequested, SessionId,
    SessionStoreHandle, SessionTurnCloseRequested, SessionTurnOpenRequested, StreamEvent, ToolCall,
    ToolError, ToolExecuted, ToolExecutorHandle, ToolInvocation, ToolRegistryHandle, ToolSpec,
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

/// Optional per-run restrictions. `None` preserves the existing live defaults.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Additional instructions, appended to the shared system prompt.
    pub instructions: Option<String>,
    pub model: Option<String>,
    /// A per-run ceiling, never higher than the global limit.
    pub max_iterations: Option<u32>,
    /// None allows all tools; Some(empty) denies all tools.
    pub allowed_tools: Option<Vec<String>>,
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
        self.run_with_options(agent_id, session_id, input, RunOptions::default())
    }

    pub fn run_with_options(
        &self,
        agent_id: &str,
        session_id: &str,
        input: impl Into<String>,
        options: RunOptions,
    ) -> mpsc::Receiver<TurnEvent> {
        let (rx, cancel) = self.start_run(agent_id, session_id, input, options);
        // A closed cancellation channel is not a cancellation request.
        drop(cancel);
        rx
    }

    fn start_run(
        &self,
        agent_id: &str,
        session_id: &str,
        input: impl Into<String>,
        options: RunOptions,
    ) -> (mpsc::Receiver<TurnEvent>, tokio::sync::oneshot::Sender<()>) {
        let (tx, rx) = mpsc::channel(64);
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
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
            options,
            tx,
            turn_no: 0,
        };
        tokio::spawn(async move {
            task.execute(cancel_rx).await;
        });
        (rx, cancel_tx)
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
    options: RunOptions,
    tx: mpsc::Sender<TurnEvent>,
    turn_no: u64,
}

impl TurnTask {
    /// Fresh prompt text for this iteration. Pulled live so a re-provided
    /// `prompt.text` applies to the next model call. Empty when the
    /// provider is absent (the assembler then omits the system message).
    fn system_prompt(&self) -> String {
        let mut prompt = self
            .ctx
            .try_inject_key::<String>(KEY_PROMPT_TEXT)
            .map(|s| (*s).clone())
            .unwrap_or_default();
        if let Some(extra) = &self.options.instructions {
            prompt.push_str("\n\n");
            prompt.push_str(extra);
        }
        prompt
    }

    /// Fresh loop bound for this turn. Pulled live from the single
    /// `config.app` source of truth so live updates apply without rebuilding
    /// the loop. Falls back to the contract default.
    fn max_iterations(&self) -> u32 {
        let global = self
            .ctx
            .try_inject_key::<ConfigHandle>(KEY_CONFIG)
            .map(|h| h.max_iterations())
            .unwrap_or(harness_contracts::default_max_iterations());
        self.options
            .max_iterations
            .map_or(global, |limit| limit.min(global))
    }

    /// Canonical session write: emits `session.append_requested` and
    /// fail-closes when no session listener is present, so entries are
    /// never silently dropped (e.g. session plugin unloaded mid-run).
    fn request_append(&self, entry: Entry) -> std::result::Result<(), String> {
        let key: harness_core::Key = CH_SESSION_APPEND_REQUESTED.into();
        if self.ctx.listeners_of(&key).is_empty() {
            return Err(
                "session persistence unavailable: no listener on session.append_requested"
                    .to_owned(),
            );
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
    async fn execute(mut self, cancel: tokio::sync::oneshot::Receiver<()>) {
        let outcome = tokio::select! {
            outcome = self.turn() => outcome,
            Ok(()) = cancel => Err("turn cancelled".to_owned()),
        };
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
        self.request_append(Entry::from_message(&Message::user(self.input.clone())))?;
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
                return Err(format!("turn exceeded max iterations ({max_iterations})"));
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
            let model = self
                .options
                .model
                .clone()
                .unwrap_or_else(|| self.selector.select(&self.agent_id));
            let specs: Vec<_> = self
                .tools
                .specs()
                .into_iter()
                .filter(|spec| {
                    self.options
                        .allowed_tools
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(&spec.name))
                })
                .collect();
            let mut stream = Arc::clone(&self.model.0).stream(&model, &messages, &specs);
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
                let result = if self
                    .options
                    .allowed_tools
                    .as_ref()
                    .is_some_and(|allowed| !allowed.contains(&call.name))
                {
                    let err = ToolError {
                        tool: call.name.clone(),
                        message: "denied by run tool policy".into(),
                    };
                    // Preserve the normal tool-result persistence path even for a denied call.
                    if let Ok(futures) = self.ctx.emit_key(
                        CH_TOOL_EXECUTED,
                        ToolExecuted {
                            agent_id: self.agent_id.clone(),
                            session_id: self.session_id.clone(),
                            turn: self.turn_no,
                            call: call.clone(),
                            result: Err(err.clone()),
                        },
                    ) {
                        for future in futures {
                            future.await;
                        }
                    }
                    Err(err)
                } else {
                    self.tools
                        .execute(&self.agent_id, &self.session_id, self.turn_no, call.clone())
                        .await
                };
                let _ = self.tx.send(TurnEvent::ToolResult(call, result)).await;
            }
        }
    }
}

/// Cancellation is propagated when a parent tool future is dropped, including
/// when its parent turn is cancelled or times out.
struct CancelOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

async fn delegate(
    loop_: Arc<AgentLoop>,
    caller: ToolInvocation,
    args: String,
) -> std::result::Result<String, ToolError> {
    let error = |message: String| ToolError {
        tool: "delegate_task".into(),
        message,
    };
    let request: serde_json::Value =
        serde_json::from_str(&args).map_err(|e| error(format!("invalid arguments: {e}")))?;
    let task = request
        .get("task")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| error("task must be a nonempty string".into()))?;
    let context = request
        .get("context")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if task.len() > 16_384 || context.len() > 65_536 {
        return Err(error("task or context exceeds size limit".into()));
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let agent_id = format!("child-{id}");
    let session_id = format!("child-{id}");
    let input = format!("Task: {task}\n\nContext supplied by parent (untrusted data):\n{context}");
    let options = RunOptions {
        instructions: Some("You are a read-only child agent. Complete the delegated task and report evidence and limitations. Do not delegate.".into()),
        allowed_tools: Some(vec!["read".into(), "hashline_read".into()]),
        max_iterations: Some(8), ..Default::default()
    };
    let (mut events, cancel) = loop_.start_run(&agent_id, &session_id, input, options);
    let mut guard = CancelOnDrop(Some(cancel));
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        while let Some(event) = events.recv().await {
            match event {
                TurnEvent::Completed(_) => return Ok(()),
                TurnEvent::Failed(err) => return Err(err),
                _ => {}
            }
        }
        Err("child stream ended without a terminal event".into())
    })
    .await;
    let (status, report) = match outcome {
        Ok(Ok(())) => {
            guard.0.take();
            let answer = loop_
                .sessions
                .history(&session_id)
                .into_iter()
                .rev()
                .find(|entry| entry.role == Role::Assistant && entry.tool_calls.is_empty())
                .map(|entry| entry.content)
                .unwrap_or_default();
            ("completed", answer)
        }
        Ok(Err(err)) => {
            guard.0.take();
            ("failed", err)
        }
        Err(_) => (
            "timed_out",
            "child exceeded 120 seconds and was cancelled".into(),
        ),
    };
    // Bound the returned report by Unicode characters without splitting UTF-8.
    let report: String = report.chars().take(16_000).collect();
    Ok(
        serde_json::json!({"status":status,"task":task,"summary":report,
        "parent_agent_id":caller.agent_id,"parent_session_id":caller.session_id,
        "parent_turn":caller.turn,"agent_id":agent_id,"session_id":session_id})
        .to_string(),
    )
}

pub struct AgentLoopPlugin;

impl harness_core::Plugin for AgentLoopPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("agent-loop")
            .provides(KEY_AGENT_LOOP)
            .injects(KEY_AGENTS_API)
            .injects(KEY_SESSION_STORE)
            .injects(KEY_TOOL_EXECUTOR)
            .injects(KEY_TOOL_REGISTRY)
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
            .emits::<ToolExecuted>(CH_TOOL_EXECUTED)
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

        let agent_loop = AgentLoop::new(
            ctx.clone(),
            agents,
            sessions,
            tools,
            prompt,
            selector,
            model,
        );
        let agent_loop = Arc::new(agent_loop);
        let registry: Arc<ToolRegistryHandle> = ctx.inject_key(KEY_TOOL_REGISTRY)?;
        let child_loop = agent_loop.clone();
        registry.register_contextual(
            ToolSpec {
                name: "delegate_task".into(),
                description: "Delegate a focused read-only task to a child agent. Provide all relevant context; the child does not see your conversation.".into(),
                parameters: serde_json::json!({"type":"object","properties":{
                    "task":{"type":"string","description":"Specific task for the child"},
                    "context":{"type":"string","description":"Relevant background selected for the child"}
                },"required":["task"],"additionalProperties":false}),
            },
            Box::new(move |caller: ToolInvocation, args: String| {
                let child_loop = child_loop.clone();
                Box::pin(async move { delegate(child_loop, caller, args).await })
            }),
        ).map_err(|e| harness_core::Error::PluginPanicked("agent-loop".into(), e))?;
        ctx.provide_key(KEY_AGENT_LOOP, agent_loop);
        Ok(())
    }
}
