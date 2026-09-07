use serde::{Deserialize, Serialize};

// ---- agent ------------------------------------------------------------------

pub type AgentId = String;
pub type SessionId = String;

/// Lifecycle states of an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// No turn in flight.
    Idle,
    /// A turn is executing (model call or tool call in progress).
    Busy,
    /// The last turn ended in an error.
    Failed,
}

// ---- conversation -----------------------------------------------------------

/// Chat role for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A conversation message exchanged with the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls the assistant requested, present on assistant messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Referenced call id, present only on tool-result messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Message {
            role: Role::System,
            content: content.into(),
            tool_calls: Vec::new(),
            call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            call_id: None,
        }
    }

    pub fn assistant_with_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Message {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            call_id: None,
        }
    }

    /// Tool-result message (`role: tool`) referencing a call id.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Message {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            call_id: Some(call_id.into()),
        }
    }
}

/// A single tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-issued id, echoed back with the result.
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string, passed through as-is.
    pub arguments: String,
}

/// Declarative description of a tool, advertised to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the parameters object.
    pub parameters: serde_json::Value,
}

// ---- session ----------------------------------------------------------------

/// One append-only entry in a session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub role: Role,
    pub content: String,
    /// Present only on assistant entries that request tool calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Present only on tool entries; references the answered call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

impl Entry {
    pub fn from_message(msg: &Message) -> Self {
        Entry {
            role: msg.role,
            content: msg.content.clone(),
            tool_calls: msg.tool_calls.clone(),
            call_id: msg.call_id.clone(),
        }
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Entry {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            call_id: Some(call_id.into()),
        }
    }
}

// ---- errors -----------------------------------------------------------------

/// Failure of a tool execution, carried in-band on `ToolExecuted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub tool: String,
    pub message: String,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tool `{}` failed: {}", self.tool, self.message)
    }
}

// ---- event payloads ---------------------------------------------------------

/// `CH_AGENT_CREATED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCreated {
    pub agent_id: AgentId,
}

/// `CH_AGENT_STATE_CHANGED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStateChanged {
    pub agent_id: AgentId,
    pub state: AgentState,
}

/// `CH_SESSION_TURN_OPENED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTurnOpened {
    pub session_id: SessionId,
    pub turn: u64,
}

/// `CH_SESSION_ENTRY_APPENDED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntryAppended {
    pub session_id: SessionId,
    pub turn: u64,
    pub entry: Entry,
}

/// `CH_SESSION_TURN_CLOSED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTurnClosed {
    pub session_id: SessionId,
    pub turn: u64,
}

/// `CH_TOOL_REGISTERED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRegistered {
    pub name: String,
}

/// `CH_TOOL_EXECUTED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecuted {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub turn: u64,
    pub call: ToolCall,
    pub result: Result<String, ToolError>,
}

/// `CH_TOOL_APPROVAL` waterfall payload: pre-execution gate for tool calls.
///
/// A future guardrail plugin registers a waterfall handler on
/// `CH_TOOL_APPROVAL`, inspects/rewrites `call`, and sets `denied = Some(reason)`
/// to veto. With no handlers the call runs unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolApproval {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub turn: u64,
    pub call: ToolCall,
    /// `Some(reason)` vetoes execution; `None` allows (possibly rewritten) `call`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<String>,
}

impl ToolApproval {
    pub fn allow(
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        turn: u64,
        call: ToolCall,
    ) -> Self {
        ToolApproval {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            turn,
            call,
            denied: None,
        }
    }

    pub fn deny(
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        turn: u64,
        call: ToolCall,
        reason: impl Into<String>,
    ) -> Self {
        ToolApproval {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            turn,
            call,
            denied: Some(reason.into()),
        }
    }

    pub fn is_denied(&self) -> bool {
        self.denied.is_some()
    }
}

/// `CH_PROMPT_ASSEMBLED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptAssembled {
    pub agent_id: AgentId,
    pub iteration: u32,
    pub prompt: String,
}

/// `CH_MODEL_SELECTED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelected {
    pub agent_id: AgentId,
    pub model: String,
}

/// `CH_TURN_STARTED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStarted {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub turn: u64,
}

/// `CH_TURN_ITERATION`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnIteration {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub turn: u64,
    pub iteration: u32,
}

/// `CH_TURN_COMPLETED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleted {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub turn: u64,
    pub iterations: u32,
}

/// `CH_TURN_FAILED`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnFailed {
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub turn: u64,
    pub error: String,
}
