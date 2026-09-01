//! Service keys (DI) and event channel keys (bus).
//!
//! Services are registered under `KEY_*`, event payloads travel on `CH_*`.
//! Channel names use the `<domain>.<event>` convention.

// ---- services ---------------------------------------------------------------

/// `Arc<AppConfig>` — provided by the config plugin.
pub const KEY_CONFIG: &str = "config.app";
/// `Arc<dyn ModelClient>` — provided by the model plugin.
pub const KEY_MODEL_CLIENT: &str = "model.chat";
/// `Arc<AgentRegistry>` — provided by the agent plugin.
pub const KEY_AGENTS: &str = "agents.registry";
/// `Arc<SessionLog>` — provided by the session plugin.
pub const KEY_SESSIONS: &str = "sessions.log";
/// `Arc<Tools>` — provided by the tools plugin.
pub const KEY_TOOLS: &str = "tools.executor";
/// `Arc<PromptAssembler>` — provided by the system-prompt plugin.
pub const KEY_PROMPT: &str = "prompt.assembler";
/// `Arc<ModelSelector>` — provided by the agent-default-model plugin.
pub const KEY_MODEL_SELECTOR: &str = "model.selector";
/// `Arc<AgentLoop>` — provided by the agent-loop plugin.
pub const KEY_AGENT_LOOP: &str = "agent_loop.run";
/// `Arc<Repl>` — provided by the repl plugin.
pub const KEY_REPL: &str = "ui.repl";

// ---- channels ---------------------------------------------------------------

/// `AgentCreated` — agent registry created a new agent.
pub const CH_AGENT_CREATED: &str = "agent.created";
/// `AgentStateChanged` — an agent's lifecycle state changed.
pub const CH_AGENT_STATE_CHANGED: &str = "agent.state_changed";

/// `SessionTurnOpened` — a session turn was opened.
pub const CH_SESSION_TURN_OPENED: &str = "session.turn_opened";
/// `SessionEntryAppended` — an entry was appended to a session.
pub const CH_SESSION_ENTRY_APPENDED: &str = "session.entry_appended";
/// `SessionTurnClosed` — a session turn was closed.
pub const CH_SESSION_TURN_CLOSED: &str = "session.turn_closed";

/// `ToolRegistered` — a tool was registered with the executor.
pub const CH_TOOL_REGISTERED: &str = "tool.registered";
/// `ToolExecuted` — a tool call finished (result carries Ok/Err).
pub const CH_TOOL_EXECUTED: &str = "tool.executed";

/// `PromptAssembled` — a system prompt was assembled (debug/telemetry).
pub const CH_PROMPT_ASSEMBLED: &str = "prompt.assembled";
/// `ModelSelected` — a model was chosen for an iteration (telemetry).
pub const CH_MODEL_SELECTED: &str = "model.selected";

/// `TurnStarted` — a loop turn began.
pub const CH_TURN_STARTED: &str = "turn.started";
/// `TurnIteration` — a loop iteration began.
pub const CH_TURN_ITERATION: &str = "turn.iteration";
/// `TurnCompleted` — a loop turn finished successfully.
pub const CH_TURN_COMPLETED: &str = "turn.completed";
/// `TurnFailed` — a loop turn aborted with an error.
pub const CH_TURN_FAILED: &str = "turn.failed";
