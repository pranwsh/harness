//! Service keys (DI) and event channel keys (bus).
//!
//! Services are registered under `KEY_*`, event payloads travel on `CH_*`.
//! Channel names use the `<domain>.<event>` convention.

// ---- services ---------------------------------------------------------------

/// `Arc<ConfigHandle>` (`Arc<dyn ConfigApi>`) — single source of truth for
/// configuration. Provided by the config plugin; consumers derive snapshots
/// (`get()`), bounds (`max_iterations()`), or mutations (`set_llm_model()`)
/// from it so live updates never fork across keys.
pub const KEY_CONFIG: &str = "config.app";

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
/// [`KEY_POPUP`](KEY_POPUP) for content plus [`KEY_MODEL_CATALOG`] and an
/// optional [`KEY_CONFIG`] (via `ConfigApi`) for persistence.
pub const KEY_MODEL_POPUP: &str = "ui.model_popup";
/// `Arc<CommandPopup>` — provided by the tui-commands plugin, injected by
/// the tui shell to route slash-command autocomplete keys. Owns its own
/// popup surface (injects nothing), so it can never observe or clobber
/// another provider's list; the shell draws at most one snapshot with the
/// model selector taking precedence.
pub const KEY_COMMAND_POPUP: &str = "ui.command_popup";
/// `Arc<SessionPopup>` — provided by the tui-sessions plugin, injected by
/// the tui shell to route `/sessions` picker keys and session switches.
/// Owns its own popup surface (injects only the session catalog), so it
/// can never observe or clobber another provider's list.
pub const KEY_SESSION_POPUP: &str = "ui.session_popup";
/// `Arc<HashStore>` — provided by the hash-base plugin, injected by the
/// hashline-read and hashline-edit plugins for line hashing and revisions.
pub const KEY_HASH_STORE: &str = "hash.store";
/// `Arc<ShellService>` — provided by the shell plugin. Kept alive for the
/// plugin lifetime so background jobs are aborted on unload via `JobManager::drop`.
pub const KEY_SHELL_SERVICE: &str = "shell.service";
/// `Arc<McpService>` — provided by the mcp plugin. Kept alive for the
/// plugin lifetime so server child processes are killed on unload via `Drop`.
pub const KEY_MCP_SERVICE: &str = "mcp.service";

// ---- service-trait handles (decoupled DI) -----------------------------------
// Each provider publishes `Arc<Handle>` here alongside its legacy concrete
// service. New consumers inject the handle and never name the provider
// crate; legacy concrete keys remain for pre-migration consumers.

/// `Arc<AgentRegistryHandle>` — trait view over the agent registry.
pub const KEY_AGENTS_API: &str = "agents.api";
/// `Arc<SessionStoreHandle>` — read + turn-bookkeeping view over sessions.
pub const KEY_SESSION_STORE: &str = "sessions.store";
/// `Arc<SessionCatalogHandle>` — catalog view over past sessions for the
/// `/sessions` picker. Provided alongside the store by the session plugin.
pub const KEY_SESSION_CATALOG: &str = "sessions.catalog";
/// `Arc<ToolExecutorHandle>` — specs + execute view over tools.
pub const KEY_TOOL_EXECUTOR: &str = "tools.executor_api";
/// `Arc<ToolRegistryHandle>` — registration view over tools.
pub const KEY_TOOL_REGISTRY: &str = "tools.registry";
/// `Arc<PromptAssemblerHandle>` — pure assembly view over system-prompt.
pub const KEY_PROMPT_ASSEMBLER: &str = "prompt.assembler_api";
/// `Arc<ModelSelectorHandle>` — per-iteration model choice view.
pub const KEY_MODEL_SELECTOR_API: &str = "model.selector_api";
/// `Arc<ModelCatalogHandle>` — rich catalog view for the `/model` popup.
pub const KEY_MODEL_CATALOG: &str = "model.catalog";
/// `Arc<ModelStreamerHandle>` — streaming view over the model client.
pub const KEY_MODEL_STREAMER: &str = "model.streamer";

// ---- channels ---------------------------------------------------------------

/// `AgentCreated` — agent registry created a new agent.
pub const CH_AGENT_CREATED: &str = "agent.created";
/// `AgentStateChanged` — an agent's lifecycle state changed.
pub const CH_AGENT_STATE_CHANGED: &str = "agent.state_changed";

/// `SessionTurnOpened` — a session turn was opened.
pub const CH_SESSION_TURN_OPENED: &str = "session.turn_opened";
/// `SessionTurnOpenRequested` — waterfall request for a new turn.
pub const CH_SESSION_TURN_OPEN_REQUESTED: &str = "session.turn_open_requested";
/// `SessionTurnCloseRequested` — one-way request to close a turn.
pub const CH_SESSION_TURN_CLOSE_REQUESTED: &str = "session.turn_close_requested";
/// `SessionEntryAppended` — an entry was appended to a session.
pub const CH_SESSION_ENTRY_APPENDED: &str = "session.entry_appended";
/// `SessionTurnClosed` — a session turn was closed.
pub const CH_SESSION_TURN_CLOSED: &str = "session.turn_closed";
/// `SessionAppendRequested` — a producer (e.g. the agent loop) asks the
/// session plugin to append one entry. The session plugin owns all
/// persistence and handles this with a sync listener, so the entry is
/// durable before the emitter resumes. User/assistant entries travel here;
/// tool results travel via `tool.executed` for the same guarantee.
pub const CH_SESSION_APPEND_REQUESTED: &str = "session.append_requested";

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
