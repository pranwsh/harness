//! Service keys (DI) and event channel keys (bus).
//!
//! Services are registered under `KEY_*`, event payloads travel on `CH_*`.
//! Channel names use the `<domain>.<event>` convention.

// ---- services ---------------------------------------------------------------

/// `Arc<AppConfig>` — provided by the config plugin.
pub const KEY_CONFIG: &str = "config.app";
/// `Arc<ConfigService>` — provided by the config plugin. General,
/// domain-agnostic read/modify/persist access to `config.toml`; consumers
/// (e.g. the `/model` bridge) mutate through it instead of touching files.
pub const KEY_CONFIG_SERVICE: &str = "config.service";
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
/// `Arc<String>` — prompt text, provided by the system-prompt plugin,
/// injected by the agent-loop plugin.
pub const KEY_PROMPT_TEXT: &str = "prompt.text";
/// `Arc<ModelSelector>` — provided by the agent-default-model plugin.
pub const KEY_MODEL_SELECTOR: &str = "model.selector";
/// `Arc<AgentLoop>` — provided by the agent-loop plugin.
pub const KEY_AGENT_LOOP: &str = "agent_loop.run";
/// `Arc<Tui>` — provided by the tui plugin.
pub const KEY_TUI: &str = "ui.tui";
/// `Arc<RendererHandle>` — provided by the tui-markdown plugin,
/// injected by the tui plugin to render assistant messages.
pub const KEY_MARKDOWN_RENDERER: &str = "markdown.renderer";
/// `Arc<Input>` — provided by the tui-input plugin, injected by the tui
/// plugin to read terminal key/mouse events.
pub const KEY_INPUT: &str = "ui.input";
/// `Arc<Popup>` — provided by the tui-popup plugin. Generic single-select
/// floating-list state only; providers push plain `String` items so the
/// popup never depends on any domain plugin. Rendered by the tui shell;
/// filled by provider plugins such as tui-model.
pub const KEY_POPUP: &str = "ui.popup";
/// `Arc<ModelPopup>` — provided by the tui-model plugin, injected by the
/// tui shell to route `/model` keys and input triggers. Injects
/// [`KEY_POPUP`](KEY_POPUP) for content plus [`KEY_MODEL_SELECTOR`] and an
/// optional [`KEY_CONFIG_SERVICE`] for persistence.
pub const KEY_MODEL_POPUP: &str = "ui.model_popup";
/// `Arc<HashStore>` — provided by the hash-base plugin, injected by the
/// hashline-read and hashline-edit plugins for line hashing and revisions.
pub const KEY_HASH_STORE: &str = "hash.store";
/// `Arc<ShellService>` — provided by the shell plugin. Kept alive for the
/// plugin lifetime so background jobs are aborted on unload via `JobManager::drop`.
pub const KEY_SHELL_SERVICE: &str = "shell.service";

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
/// `ToolApproval` — waterfall gate before a tool runs. Handlers may rewrite
/// `call` or set `denied` to veto. No handlers = allow as-is.
pub const CH_TOOL_APPROVAL: &str = "tool.approval";

/// `PromptAssembled` — a system prompt was assembled (debug/telemetry).
pub const CH_PROMPT_ASSEMBLED: &str = "prompt.assembled";
/// `ModelSelected` — a model was chosen for an iteration (telemetry).
pub const CH_MODEL_SELECTED: &str = "model.selected";

/// `LlmRequestHeaders` — waterfall gate before an LLM HTTP request is sent.
/// Handlers may rewrite `headers` or set `denied` to veto. No handlers =
/// send as seeded by the model plugin.
pub const CH_LLM_REQUEST_HEADERS: &str = "llm.request_headers";

/// `LlmResponseHeaders` — an LLM HTTP response arrived (post hook,
/// observation only).
pub const CH_LLM_RESPONSE_HEADERS: &str = "llm.response_headers";

/// `TurnStarted` — a loop turn began.
pub const CH_TURN_STARTED: &str = "turn.started";
/// `TurnIteration` — a loop iteration began.
pub const CH_TURN_ITERATION: &str = "turn.iteration";
/// `TurnCompleted` — a loop turn finished successfully.
pub const CH_TURN_COMPLETED: &str = "turn.completed";
/// `TurnFailed` — a loop turn aborted with an error.
pub const CH_TURN_FAILED: &str = "turn.failed";
