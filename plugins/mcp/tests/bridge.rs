//! End-to-end: fake MCP stdio server -> `McpPlugin` -> tool registry.
//!
//! The fake server is a small python3 script speaking JSON-RPC 2.0 over
//! stdio (`initialize`, `tools/list`, `tools/call` with echo/fail/slow).
//! Each test writes its own script copy so parallel tests never share a
//! process.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use harness_contracts::{KEY_MCP_SERVICE, KEY_TOOLS, ToolCall};
use harness_mcp::{McpPlugin, McpService};

const FAKE_SERVER: &str = r#"
import sys, json, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if "method" in msg and "id" not in msg:
        continue  # notification: no reply
    mid = msg.get("id")
    m = msg.get("method")
    if m == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "serverInfo": {"name": "fake", "version": "0"}}})
    elif m == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [
            {"name": "echo", "description": "Echoes text.",
             "inputSchema": {"type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]}},
            {"name": "fail", "description": "Always fails.",
             "inputSchema": {"type": "object"}},
            {"name": "slow", "description": "Sleeps.",
             "inputSchema": {"type": "object"}},
        ]}})
    elif m == "tools/call":
        p = msg.get("params", {})
        name = p.get("name")
        args = p.get("arguments", {})
        if name == "echo":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": args.get("text", "")}],
                "isError": False}})
        elif name == "fail":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": "kaput"}],
                "isError": True}})
        elif name == "slow":
            time.sleep(30)
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": "late"}],
                "isError": False}})
        else:
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32602, "message": "unknown tool"}})
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "unknown method"}})
"#;

fn write_fake_server(tag: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "harness-mcp-fake-{}-{}-{tag}.py",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, FAKE_SERVER).unwrap();
    path.display().to_string()
}

fn config_with_servers(extra: &str) -> String {
    format!(
        "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"a\"\n{extra}"
    )
}

fn call(name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: args.into(),
    }
}

async fn wait_for_tool(tools: &Arc<harness_tools::Tools>, name: &str) {
    for _ in 0..200 {
        if tools.specs().iter().any(|s| s.name == name) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "tool `{name}` never appeared; have: {:?}",
        tools
            .specs()
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn bridge_registers_and_executes() {
    let script = write_fake_server("basic");
    let raw = config_with_servers(&format!(
        "[mcp.servers.fake]\ncommand = \"python3\"\nargs = [\"{script}\"]\n"
    ));
    let ctx = harness_core::Context::root();
    ctx.load(harness_config::ConfigPlugin::from_toml(&raw).unwrap())
        .unwrap();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(McpPlugin).unwrap();

    let tools: Arc<harness_tools::Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
    wait_for_tool(&tools, "mcp__fake__echo").await;

    // Schema passes through verbatim.
    let spec = tools
        .specs()
        .into_iter()
        .find(|s| s.name == "mcp__fake__echo")
        .unwrap();
    assert_eq!(
        spec.parameters["properties"]["text"]["type"],
        serde_json::json!("string"),
        "{:?}",
        spec.parameters
    );

    // Echo roundtrip through the registry (approval + tool.executed free).
    let out = tools
        .execute("a", "s", 1, call("mcp__fake__echo", r#"{"text":"hi"}"#))
        .await
        .unwrap();
    assert_eq!(out, "hi");

    // isError:true surfaces as a tool error, not a transport failure.
    let err = tools
        .execute("a", "s", 1, call("mcp__fake__fail", "{}"))
        .await
        .unwrap_err();
    assert_eq!(err.tool, "mcp__fake__fail");
    assert!(err.message.contains("kaput"), "{}", err.message);

    let svc: Arc<McpService> = ctx.inject_key(KEY_MCP_SERVICE).unwrap();
    assert_eq!(svc.live_servers().await, vec!["fake".to_owned()]);
    assert_eq!(svc.tool_count(), 3);
    svc.shutdown().await;
    std::fs::remove_file(&script).ok();
}

#[tokio::test]
async fn bad_server_does_not_block_good_server() {
    let script = write_fake_server("iso");
    let raw = config_with_servers(&format!(
        "[mcp.servers.bad]\ncommand = \"definitely-not-a-real-binary-xyz\"\n\
         [mcp.servers.good]\ncommand = \"python3\"\nargs = [\"{script}\"]\n"
    ));
    let ctx = harness_core::Context::root();
    ctx.load(harness_config::ConfigPlugin::from_toml(&raw).unwrap())
        .unwrap();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(McpPlugin).unwrap();

    let tools: Arc<harness_tools::Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
    wait_for_tool(&tools, "mcp__good__echo").await;
    let out = tools
        .execute("a", "s", 1, call("mcp__good__echo", r#"{"text":"ok"}"#))
        .await
        .unwrap();
    assert_eq!(out, "ok");

    let svc: Arc<McpService> = ctx.inject_key(KEY_MCP_SERVICE).unwrap();
    assert_eq!(svc.live_servers().await, vec!["good".to_owned()]);
    svc.shutdown().await;
    std::fs::remove_file(&script).ok();
}

#[tokio::test]
async fn per_call_timeout_is_clean_error() {
    let script = write_fake_server("slow");
    let raw = config_with_servers(&format!(
        "[mcp.servers.slowpoke]\ncommand = \"python3\"\nargs = [\"{script}\"]\ntimeout_ms = 300\n"
    ));
    let ctx = harness_core::Context::root();
    ctx.load(harness_config::ConfigPlugin::from_toml(&raw).unwrap())
        .unwrap();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(McpPlugin).unwrap();

    let tools: Arc<harness_tools::Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
    wait_for_tool(&tools, "mcp__slowpoke__slow").await;
    let err = tools
        .execute("a", "s", 1, call("mcp__slowpoke__slow", "{}"))
        .await
        .unwrap_err();
    assert_eq!(err.tool, "mcp__slowpoke__slow");
    assert!(err.message.contains("timed out"), "{}", err.message);

    let svc: Arc<McpService> = ctx.inject_key(KEY_MCP_SERVICE).unwrap();
    svc.shutdown().await;
    std::fs::remove_file(&script).ok();
}

#[tokio::test]
async fn unknown_tool_and_bad_arguments_are_clean_errors() {
    let raw = config_with_servers("");
    let ctx = harness_core::Context::root();
    ctx.load(harness_config::ConfigPlugin::from_toml(&raw).unwrap())
        .unwrap();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(McpPlugin).unwrap();

    let svc: Arc<McpService> = ctx.inject_key(KEY_MCP_SERVICE).unwrap();
    assert!(svc.enabled_servers().is_empty());
    assert!(svc.live_servers().await.is_empty());

    let err = svc.execute("mcp__nope__x", "{}").await.unwrap_err();
    assert_eq!(err.tool, "mcp__nope__x");
    assert_eq!(err.message, "unknown tool");
    svc.shutdown().await;
}

#[tokio::test]
async fn disabled_server_is_never_spawned() {
    let raw = config_with_servers(
        "[mcp.servers.off]\ncommand = \"definitely-not-a-real-binary-xyz\"\ndisabled = true\n",
    );
    let ctx = harness_core::Context::root();
    ctx.load(harness_config::ConfigPlugin::from_toml(&raw).unwrap())
        .unwrap();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(McpPlugin).unwrap();

    // Give discovery a chance to (incorrectly) spawn; nothing should appear.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let tools: Arc<harness_tools::Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
    assert!(tools.specs().is_empty(), "{:?}", tools.specs());
    let svc: Arc<McpService> = ctx.inject_key(KEY_MCP_SERVICE).unwrap();
    assert!(svc.live_servers().await.is_empty());
    svc.shutdown().await;
}
