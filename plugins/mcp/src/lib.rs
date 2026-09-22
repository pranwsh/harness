//! `mcp`: Model Context Protocol (stdio) tool bridge.
//!
//! Each `[mcp.servers.<name>]` entry spawns one persistent child process
//! speaking JSON-RPC 2.0 over stdio. Discovered tools are registered on
//! `tools.registry` as `mcp__<server>__<tool>` and execute via
//! `tools/call`, so the agent loop, approval waterfall (`tool.approval`),
//! logging (`tool.executed`), and persistence all apply unchanged.
//!
//! Discovery is async and fail-isolated per server: one bad command never
//! blocks the others, and tools appear on the next loop iteration once
//! registered. No policy lives here: any configured command runs with the
//! agent's environment. Run trusted servers only, preferably in a container.

mod protocol;
mod service;
mod session;

pub use service::McpService;

use std::sync::Arc;

use harness_contracts::{
    ConfigHandle, KEY_CONFIG, KEY_MCP_SERVICE, KEY_TOOL_REGISTRY, ToolRegistryHandle,
};
use harness_core::{Context, Result};

/// MCP bridge plugin. Reads `[mcp]` from `config.app` via DI and injects
/// `tools.registry` (same shape as `shell` / `hashline-read`).
///
/// ```rust,no_run
/// # use harness_mcp::McpPlugin;
/// let plugin = McpPlugin;
/// ```
#[derive(Default, Debug, Clone, Copy)]
pub struct McpPlugin;

impl harness_core::Plugin for McpPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("mcp")
            .provides(KEY_MCP_SERVICE)
            .injects(KEY_TOOL_REGISTRY)
            .injects(KEY_CONFIG)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let registry: Arc<ToolRegistryHandle> = ctx.inject_key(KEY_TOOL_REGISTRY)?;
        let handle: Arc<ConfigHandle> = ctx.inject_key(KEY_CONFIG)?;
        let svc = Arc::new(McpService::new(handle.get().mcp.clone()));
        // Keep the service alive for the plugin lifetime; dropping it kills
        // server children via `kill_on_drop`. Stored under `KEY_MCP_SERVICE`
        // owned by this plugin so unload cleans it up.
        ctx.provide_key(KEY_MCP_SERVICE, Arc::clone(&svc));
        // Async discovery runs after all fallible work, so there is nothing
        // to roll back. Requires a tokio runtime (present in the harness
        // binary and in `#[tokio::test]`s); without one the plugin stays
        // loaded but discovers nothing.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(service::discover(Arc::clone(&svc), registry));
        }
        Ok(())
    }
}
