//! `McpService`: owns all server sessions and bridges MCP tools into the
//! harness tool registry.
//!
//! Discovery is fully async and fail-isolated per server: one bad command
//! never blocks the others. Tool names are prefixed `mcp__<server>__<tool>`
//! (reversible via the `exposed` table) so collisions with built-in tools
//! are impossible and routing needs no parsing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use harness_contracts::{BoxFuture, McpConfig, ToolError, ToolRegistryHandle, ToolSpec};
use tokio::sync::Mutex as AsyncMutex;

use crate::session::McpSession;

/// Cap for one MCP tool result surfaced to the model.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 65536;

/// Runtime service behind the MCP bridge. Clone via `Arc`.
pub struct McpService {
    config: McpConfig,
    max_output_bytes: usize,
    sessions: AsyncMutex<HashMap<String, Arc<McpSession>>>,
    /// Exposed tool name -> (server, remote tool).
    exposed: Mutex<HashMap<String, (String, String)>>,
    /// Watcher tasks (one per live server). Aborted on `shutdown`/`Drop`
    /// so unload never leaves a task re-registering tools behind.
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl McpService {
    pub fn new(config: McpConfig) -> Self {
        McpService {
            config,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            sessions: AsyncMutex::new(HashMap::new()),
            exposed: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Names of enabled servers (configured, not `disabled`).
    pub fn enabled_servers(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .config
            .servers
            .iter()
            .filter(|(_, c)| !c.disabled)
            .map(|(n, _)| n.clone())
            .collect();
        out.sort();
        out
    }

    /// Names of live (successfully connected) servers.
    pub async fn live_servers(&self) -> Vec<String> {
        let mut out: Vec<String> = self.sessions.lock().await.keys().cloned().collect();
        out.sort();
        out
    }

    /// Number of bridged (registered) tools.
    pub fn tool_count(&self) -> usize {
        self.exposed.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Graceful shutdown: aborts watchers, then closes every session
    /// (EOF + bounded grace, then task abort which kills the child via
    /// `kill_on_drop`).
    pub async fn shutdown(&self) {
        if let Ok(tasks) = self.tasks.lock() {
            for t in tasks.iter() {
                t.abort();
            }
        }
        let sessions: Vec<Arc<McpSession>> =
            self.sessions.lock().await.values().cloned().collect();
        for s in sessions {
            s.shutdown().await;
        }
    }

    /// Abort every watcher task. Sync so `Drop` can call it; aborting a
    /// watcher drops its session `Arc`, and the last drop kills the server
    /// via `kill_on_drop`.
    fn abort_all_sync(&self) {
        if let Ok(tasks) = self.tasks.lock() {
            for t in tasks.iter() {
                t.abort();
            }
        }
    }

    /// Executes one bridged tool by exposed name. Called from registry
    /// handler closures; resolves routing via the `exposed` table.
    pub async fn execute(&self, exposed: &str, args: impl Into<String>) -> Result<String, ToolError> {
        let err = |msg: String| ToolError {
            tool: exposed.to_owned(),
            message: msg,
        };
        let (server, tool) = self
            .exposed
            .lock()
            .map_err(|_| err("tool table poisoned".to_owned()))?
            .get(exposed)
            .cloned()
            .ok_or_else(|| err("unknown tool".to_owned()))?;
        let args = args.into();
        let sess = self
            .sessions
            .lock()
            .await
            .get(&server)
            .cloned()
            .ok_or_else(|| err(format!("server `{server}` is not connected")))?;
        let args_value: serde_json::Value = if args.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&args).map_err(|e| err(format!("invalid arguments: {e}")))?
        };
        if !args_value.is_object() {
            return Err(err("arguments must be a JSON object".to_owned()));
        }
        sess.call_tool(&tool, args_value)
            .await
            .map_err(err)
    }
}

impl Drop for McpService {
    fn drop(&mut self) {
        self.abort_all_sync();
    }
}

/// Exposed tool name for a server/tool pair. Server segments are
/// sanitized (non-`[a-zA-Z0-9_-]` → `_`); remote tool names pass through
/// verbatim (routing uses the table, never parsing).
pub fn exposed_name(server: &str, tool: &str) -> String {
    let clean: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("mcp__{clean}__{tool}")
}

/// Builds the advertised [`ToolSpec`] for one remote tool. The MCP
/// `inputSchema` passes through verbatim when it is an object, else an
/// empty object schema (model must send `{}`).
pub fn tool_spec(server: &str, name: &str, description: Option<&str>, input_schema: &serde_json::Value) -> ToolSpec {
    let parameters = if input_schema.is_object() {
        input_schema.clone()
    } else {
        serde_json::json!({ "type": "object" })
    };
    let desc = match description {
        Some(d) if !d.trim().is_empty() => {
            format!("MCP tool `{name}` from server `{server}`: {d}")
        }
        _ => format!("MCP tool `{name}` from server `{server}`."),
    };
    ToolSpec {
        name: exposed_name(server, name),
        description: desc,
        parameters,
    }
}

/// Connects every enabled server concurrently, registers their tools, and
/// spawns a `list_changed` watcher per live server. Fail-isolated: one
/// server's spawn/handshake/list failure is skipped, the rest proceed.
pub async fn discover(service: Arc<McpService>, registry: Arc<ToolRegistryHandle>) {
    let servers = service.enabled_servers();
    let max_out = service.max_output_bytes;
    let mut tasks = Vec::with_capacity(servers.len());
    for server in servers {
        let svc = Arc::clone(&service);
        let reg = Arc::clone(&registry);
        tasks.push(tokio::spawn(async move {
            connect_and_register(svc, reg, server, max_out).await;
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
}

async fn connect_and_register(
    service: Arc<McpService>,
    registry: Arc<ToolRegistryHandle>,
    server: String,
    max_out: usize,
) {
    let cfg = match service.config.servers.get(&server) {
        Some(c) => c.clone(),
        None => return,
    };
    let sess = match McpSession::spawn(server.clone(), &cfg, max_out).await {
        Ok(s) => s,
        Err(_) => return, // fail-quiet per server: never blocks the rest.
    };
    let tools = match sess.list_tools().await {
        Ok(t) => t,
        Err(_) => return,
    };
    service
        .sessions
        .lock()
        .await
        .insert(server.clone(), Arc::clone(&sess));
    register_all(&service, &registry, &server, &tools);
    // Watch for tool-list changes for the process lifetime. Tracked on the
    // service so `shutdown`/`Drop` aborts it with everything else.
    let svc = Arc::clone(&service);
    let watcher = tokio::spawn(async move {
        loop {
            sess.changed_notified().await;
            let Ok(tools) = sess.list_tools().await else {
                continue;
            };
            register_all(&svc, &registry, &server, &tools);
        }
    });
    if let Ok(mut tasks) = service.tasks.lock() {
        tasks.push(watcher.abort_handle());
    }
}

fn register_all(
    service: &Arc<McpService>,
    registry: &Arc<ToolRegistryHandle>,
    server: &str,
    tools: &[crate::protocol::ToolInfo],
) {
    for t in tools {
        let spec = tool_spec(server, &t.name, t.description.as_deref(), &t.input_schema);
        let name = spec.name.clone();
        // Reserve the exposed name first: duplicate registrations (same
        // tool re-listed, or cross-server collision) are skipped.
        let fresh = {
            let mut exposed = match service.exposed.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if exposed.contains_key(&name) {
                false
            } else {
                exposed.insert(name.clone(), (server.to_owned(), t.name.clone()));
                true
            }
        };
        if !fresh {
            continue;
        }
        let svc = Arc::clone(service);
        let owned = name.clone();
        let res = registry.register(
            spec,
            Box::new(move |args| {
                let svc = Arc::clone(&svc);
                let owned = owned.clone();
                Box::pin(async move { svc.execute(&owned, args).await })
                    as BoxFuture<Result<String, ToolError>>
            }),
        );
        if res.is_err() {
            // Registry already had the name (e.g. restarted server with a
            // stale entry): roll back the reservation.
            if let Ok(mut exposed) = service.exposed.lock() {
                exposed.remove(&name);
            }
        }
    }
}
