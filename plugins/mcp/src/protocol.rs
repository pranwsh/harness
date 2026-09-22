//! JSON-RPC 2.0 + MCP wire types (stdio transport).
//!
//! Minimal surface for tool bridging: `initialize`, `tools/list` (with
//! cursor pagination), and `tools/call`. Notifications from the server
//! (e.g. `notifications/tools/list_changed`) carry no `id` and are never
//! answered.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// MCP protocol version we advertise in `initialize`. The server's answer
/// is accepted as-is (lenient handshake: capability negotiation beyond
/// tools is out of scope for this bridge).
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Outgoing JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: Value,
}

impl RpcRequest {
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        RpcRequest {
            jsonrpc: "2.0".to_owned(),
            id,
            method: method.to_owned(),
            params,
        }
    }
}

/// Outgoing JSON-RPC 2.0 notification (no `id`, no reply expected).
#[derive(Debug, Clone, Serialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

impl RpcNotification {
    pub fn new(method: &str, params: Value) -> Self {
        RpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: method.to_owned(),
            params,
        }
    }
}

/// Incoming JSON-RPC 2.0 response. Exactly one of `result` / `error` is
/// present on a well-formed reply; anything else is a protocol error.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub result: Option<Value>,
    pub error: Option<RpcError>,
}

/// JSON-RPC 2.0 error payload.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[allow(dead_code)]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn internal(message: impl Into<String>) -> Self {
        RpcError {
            code: -32603,
            message: message.into(),
            data: None,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

/// One tool advertisement from `tools/list`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema of the arguments object. Passed through verbatim into
    /// the advertised [`harness_contracts::ToolSpec`]; defaulted to an
    /// empty object schema when absent or malformed.
    #[serde(default)]
    pub input_schema: Value,
}

/// `tools/list` result page.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsListResult {
    #[serde(default)]
    pub tools: Vec<ToolInfo>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// One entry of a `tools/call` result payload. Unknown fields (image
/// blobs, embedded resources, …) are ignored at parse time and rendered
/// as an omission marker by the caller.
#[derive(Debug, Clone, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

/// `tools/call` result payload.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallResult {
    #[serde(default)]
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub is_error: Option<bool>,
}

/// `initialize` params. We declare no capabilities beyond the tools
/// bridge; servers must not require more.
pub fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "harness", "version": "0.1.0" },
    })
}

/// Extracts a numeric response id from a raw message value. String ids
/// that parse as `u64` match too; anything else is not a response we can
/// route (MCP servers echo the numeric ids we send).
pub fn response_id(v: &Value) -> Option<u64> {
    match v.get("id") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse::<u64>().ok(),
        _ => None,
    }
}

/// True for a server→client notification: a `method` with no usable `id`.
/// `tools/list` responses always carry `result`, so id-less `result`
/// payloads are ignored by the reader (protocol violation, fail-quiet).
pub fn is_notification(v: &Value) -> bool {
    v.get("method").and_then(Value::as_str).is_some() && response_id(v).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_id_routes_numbers_and_numeric_strings() {
        assert_eq!(response_id(&serde_json::json!({"id": 7})), Some(7));
        assert_eq!(response_id(&serde_json::json!({"id": "42"})), Some(42));
        assert_eq!(response_id(&serde_json::json!({"method": "ping"})), None);
        assert_eq!(response_id(&serde_json::json!({"id": "abc"})), None);
        assert_eq!(response_id(&serde_json::json!({})), None);
    }

    #[test]
    fn notifications_have_method_without_routable_id() {
        assert!(is_notification(
            &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        ));
        assert!(!is_notification(
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "x", "params": {}})
        ));
        assert!(!is_notification(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}})));
    }

    #[test]
    fn initialize_params_advertise_tools_only() {
        let p = initialize_params();
        assert_eq!(p["protocolVersion"], serde_json::json!(PROTOCOL_VERSION));
        assert_eq!(p["capabilities"], serde_json::json!({}));
    }
}
