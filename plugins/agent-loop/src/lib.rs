use std::sync::Arc;

use harness_contracts::{
    AgentState, CH_TURN_COMPLETED, CH_TURN_FAILED, CH_TURN_ITERATION, CH_TURN_STARTED, Entry,
    KEY_AGENT_LOOP, KEY_AGENTS, KEY_CONFIG, KEY_MODEL_CLIENT, KEY_MODEL_SELECTOR, KEY_PROMPT,
    KEY_PROMPT_TEXT, KEY_SESSIONS, KEY_TOOLS, Message, SessionId, ToolCall, TurnCompleted,
    TurnFailed, TurnIteration, TurnStarted,
};
use harness_core::{Context, Result};
use tokio::sync::mpsc;

use harness_agent::AgentRegistry;
use harness_config::AppConfig;
use harness_model::{ModelClient, ModelClientHandle, StreamEvent};
use harness_session::SessionLog;
use harness_system_prompt::PromptAssembler;
use harness_tools::Tools;

use harness_agent_default_model::ModelSelector;

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
/// All continuation decisions are awaited service calls — events emitted on
/// the bus are pure notifications and never drive control flow, so listener
/// registration order can never affect correctness.
pub struct AgentLoop {
    ctx: Context,
    agents: Arc<AgentRegistry>,
    sessions: Arc<SessionLog>,
    tools: Arc<Tools>,
    prompt: Arc<PromptAssembler>,
    selector: Arc<ModelSelector>,
    model: Arc<ModelClientHandle>,
    system_prompt: String,
    max_iterations: u32,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: Context,
        agents: Arc<AgentRegistry>,
        sessions: Arc<SessionLog>,
        tools: Arc<Tools>,
        prompt: Arc<PromptAssembler>,
        selector: Arc<ModelSelector>,
        model: Arc<ModelClientHandle>,
        system_prompt: String,
        max_iterations: u32,
    ) -> Self {
        AgentLoop {
            ctx,
            agents,
            sessions,
            tools,
            prompt,
            selector,
            model,
            system_prompt,
            max_iterations,
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
            system_prompt: self.system_prompt.clone(),
            max_iterations: self.max_iterations,
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
    agents: Arc<AgentRegistry>,
    sessions: Arc<SessionLog>,
    tools: Arc<Tools>,
    prompt: Arc<PromptAssembler>,
    selector: Arc<ModelSelector>,
    model: Arc<ModelClientHandle>,
    system_prompt: String,
    max_iterations: u32,
    agent_id: String,
    session_id: SessionId,
    input: String,
    tx: mpsc::Sender<TurnEvent>,
    turn_no: u64,
}

impl TurnTask {
    async fn execute(mut self) {
        let outcome = self.turn().await;
        match outcome {
            Ok(iterations) => {
                self.agents.set_state(&self.agent_id, AgentState::Idle);
                self.sessions.end_turn(&self.session_id, self.turn_no);
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
                self.sessions.end_turn(&self.session_id, self.turn_no);
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
        // Ensure the agent exists, open the turn, record the user input.
        self.agents.get_or_create(&self.agent_id);
        self.agents.set_state(&self.agent_id, AgentState::Busy);
        self.turn_no = self.sessions.begin_turn(&self.session_id);
        self.sessions.append(
            &self.session_id,
            self.turn_no,
            Entry::from_message(&Message::user(self.input.clone())),
        );
        let _ = self.tx.send(TurnEvent::Started).await;
        let _ = self.ctx.emit_key(
            CH_TURN_STARTED,
            TurnStarted {
                agent_id: self.agent_id.clone(),
                session_id: self.session_id.clone(),
                turn: self.turn_no,
            },
        );

        let system_prompt = self.system_prompt.clone();

        let mut iterations = 0u32;
        loop {
            iterations += 1;
            if iterations > self.max_iterations {
                return Err(format!(
                    "turn exceeded max iterations ({})",
                    self.max_iterations
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

            // 1. Assemble prompt from current history.
            let history = self.sessions.history(&self.session_id);
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
            let mut stream = Arc::clone(&self.model).stream(&model, &messages, &self.tools.specs());
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

            // 3. Record the assistant message (deltas already displayed).
            self.sessions
                .append(&self.session_id, self.turn_no, Entry::from_message(&reply));

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
            .injects(KEY_AGENTS)
            .injects(KEY_SESSIONS)
            .injects(KEY_TOOLS)
            .injects(KEY_PROMPT)
            .injects(KEY_PROMPT_TEXT)
            .injects(KEY_MODEL_SELECTOR)
            .injects(KEY_MODEL_CLIENT)
            .injects(KEY_CONFIG)
            .emits::<TurnStarted>(CH_TURN_STARTED)
            .emits::<TurnIteration>(CH_TURN_ITERATION)
            .emits::<TurnCompleted>(CH_TURN_COMPLETED)
            .emits::<TurnFailed>(CH_TURN_FAILED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let agents: Arc<AgentRegistry> = ctx.inject_key(KEY_AGENTS)?;
        let sessions: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS)?;
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS)?;
        let prompt: Arc<PromptAssembler> = ctx.inject_key(KEY_PROMPT)?;
        let system_prompt: Arc<String> = ctx.inject_key(KEY_PROMPT_TEXT)?;
        let selector: Arc<ModelSelector> = ctx.inject_key(KEY_MODEL_SELECTOR)?;
        let model: Arc<ModelClientHandle> = ctx.inject_key(KEY_MODEL_CLIENT)?;
        let config: Arc<AppConfig> = ctx.inject_key(KEY_CONFIG)?;

        let agent_loop = AgentLoop::new(
            ctx.clone(),
            agents,
            sessions,
            tools,
            prompt,
            selector,
            model,
            (*system_prompt).clone(),
            config.agent.max_iterations,
        );
        ctx.provide_key(KEY_AGENT_LOOP, Arc::new(agent_loop));
        Ok(())
    }
}
