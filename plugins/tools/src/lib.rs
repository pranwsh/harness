use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use harness_contracts::{
    CH_TOOL_EXECUTED, CH_TOOL_REGISTERED, KEY_TOOLS, ToolCall, ToolError, ToolExecuted,
    ToolRegistered, ToolSpec,
};
use harness_core::{Context, Result};
use serde_json::json;

/// A tool implementation: raw arguments JSON in, string result out.
pub type ToolHandler = Arc<
    dyn Fn(&str) -> BoxFuture<Result<String, ToolError>> + Send + Sync,
>;

type BoxFuture<T> = futures::future::BoxFuture<'static, T>;

/// Registry and executor of tools.
///
/// `execute` runs the handler, emits `tool.executed` (Ok or Err in-band), and
/// returns the result to the caller. Async handler futures are joined so the
/// emission — including the session plugin's sync listener — completes before
/// `execute` returns: the loop never observes a stale log.
pub struct Tools {
    ctx: Context,
    tools: Mutex<HashMap<String, (ToolSpec, ToolHandler)>>,
}

impl Tools {
    pub fn new(ctx: Context) -> Self {
        Tools {
            ctx,
            tools: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a tool, rejecting duplicates and name collisions.
    pub fn register(
        &self,
        spec: ToolSpec,
        handler: impl Fn(&str) -> BoxFuture<Result<String, ToolError>> + Send + Sync + 'static,
    ) -> Result<()> {
        {
            let mut tools = self.lock();
            if tools.contains_key(&spec.name) {
                return Err(harness_core::Error::ServiceConflict {
                    key: spec.name.clone().into(),
                    provider: "tools".to_owned(),
                });
            }
            tools.insert(spec.name.clone(), (spec.clone(), Arc::new(handler)));
        }
        let _ = self
            .ctx
            .emit_key(CH_TOOL_REGISTERED, ToolRegistered { name: spec.name });
        Ok(())
    }

    /// Specs of all registered tools, for advertising to the model.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.lock().values().map(|(spec, _)| spec.clone()).collect()
    }

    /// Executes one call, emits `tool.executed`, returns the result.
    pub async fn execute(
        &self,
        agent_id: &str,
        session_id: &str,
        turn: u64,
        call: ToolCall,
    ) -> Result<String, ToolError> {
        let result = self.run(&call).await;

        let event = ToolExecuted {
            agent_id: agent_id.to_owned(),
            session_id: session_id.to_owned(),
            turn,
            call: call.clone(),
            result: result.clone(),
        };
        // Sync listeners (session append) run inline; async ones are joined
        // so the emitted history is consistent before we return.
        let futs = self.ctx.emit_key(CH_TOOL_EXECUTED, event);
        if let Ok(futs) = futs {
            for fut in futs {
                fut.await;
            }
        }
        result
    }

    async fn run(&self, call: &ToolCall) -> Result<String, ToolError> {
        let handler = {
            let tools = self.lock();
            tools
                .get(&call.name)
                .map(|(_, h)| Arc::clone(h))
                .ok_or_else(|| ToolError {
                    tool: call.name.clone(),
                    message: "unknown tool".to_owned(),
                })
        }?;
        handler(&call.arguments).await
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (ToolSpec, ToolHandler)>> {
        self.tools.lock().expect("tools registry poisoned")
    }
}

fn tool_err(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError {
        tool: tool.to_owned(),
        message: message.into(),
    }
}

/// Builtin `read_file(path)` tool.
fn read_file_handler(args: &str) -> BoxFuture<Result<String, ToolError>> {
    let path = match parse_path(args) {
        Ok(path) => path,
        Err(err) => return Box::pin(async move { Err(err) }),
    };
    Box::pin(async move {
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| tool_err("read_file", format!("cannot read {path}: {e}")))
    })
}

fn parse_path(args: &str) -> std::result::Result<String, ToolError> {
    #[derive(serde::Deserialize)]
    struct Args {
        path: String,
    }
    let args: Args = serde_json::from_str(args)
        .map_err(|e| tool_err("read_file", format!("invalid arguments: {e}")))?;
    if args.path.trim().is_empty() {
        return Err(tool_err("read_file", "path must not be empty"));
    }
    Ok(args.path)
}

pub fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a UTF-8 text file from disk and return its contents.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or cwd-relative file path" }
            },
            "required": ["path"]
        }),
    }
}

pub struct ToolsPlugin {
    /// Extra tools registered at build time (builtins are always added).
    builtins: Vec<(ToolSpec, ToolHandler)>,
}

impl ToolsPlugin {
    pub fn new() -> Self {
        ToolsPlugin { builtins: Vec::new() }
    }
}

impl Default for ToolsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl harness_core::Plugin for ToolsPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tools")
            .provides(KEY_TOOLS)
            .emits::<ToolRegistered>(CH_TOOL_REGISTERED)
            .emits::<ToolExecuted>(CH_TOOL_EXECUTED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let tools = Arc::new(Tools::new(ctx.clone()));
        ctx.provide_key(KEY_TOOLS, tools.clone());

        let mut builtins: Vec<(ToolSpec, ToolHandler)> = self.builtins.clone();
        builtins.push((read_file_spec(), Arc::new(read_file_handler)));
        for (spec, handler) in builtins {
            tools.register(spec, move |args| handler(args))?;
        }
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::Role;

    fn echo_spec() -> ToolSpec {
        ToolSpec {
            name: "echo".to_owned(),
            description: "Echoes input.".to_owned(),
            parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        }
    }

    fn echo_handler(args: &str) -> BoxFuture<Result<String, ToolError>> {
        let text = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| v.get("text").cloned())
            .map(|v| v.as_str().unwrap_or_default().to_owned());
        Box::pin(async move {
            text.map(Ok)
                .unwrap_or_else(|| Err(tool_err("echo", "missing text")))
        })
    }

    #[tokio::test]
    async fn execute_runs_handler_and_emits() {
        let ctx = Context::root();
        ctx.load(ToolsPlugin::new()).unwrap();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        tools
            .register(echo_spec(), echo_handler)
            .unwrap();

        let executed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        ctx.on_sync_key::<ToolExecuted, _>(CH_TOOL_EXECUTED, {
            let n = executed.clone();
            move |_| {
                n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .unwrap();

        let call = ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: r#"{"text":"hi"}"#.into(),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert_eq!(out, "hi");
        assert_eq!(executed.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(tools.specs().len(), 2); // echo + read_file builtin
    }

    #[tokio::test]
    async fn unknown_tool_fails_with_error_event() {
        let ctx = Context::root();
        ctx.load(ToolsPlugin::new()).unwrap();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();

        let call = ToolCall {
            id: "c2".into(),
            name: "nope".into(),
            arguments: "{}".into(),
        };
        let err = tools.execute("a", "s", 1, call).await.unwrap_err();
        assert_eq!(err.tool, "nope");
        assert_eq!(err.message, "unknown tool");
    }

    #[tokio::test]
    async fn duplicate_registration_is_rejected() {
        let ctx = Context::root();
        ctx.load(ToolsPlugin::new()).unwrap();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        tools
            .register(echo_spec(), echo_handler)
            .unwrap();
        assert!(tools.register(echo_spec(), echo_handler).is_err());
    }

    #[tokio::test]
    async fn read_file_builtin_reads_disk() {
        let ctx = Context::root();
        ctx.load(ToolsPlugin::new()).unwrap();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();

        let call = ToolCall {
            id: "c3".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"Cargo.toml"}"#.into(),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.contains("harness-tools"));
        let _ = Role::System;
    }
}
