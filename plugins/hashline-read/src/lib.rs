//! `hashline_read`: mmap-backed file read with stable per-line hashes.
//!
//! Returns a `¶path#REV:{rev}` header (plus a `lines a-b of N` suffix when
//! paginated) then `{line}:{hash}|{content}` per line. Line numbers are
//! absolute, so hashes work directly with `hashline_edit`.

mod args;
mod render;

use std::sync::Arc;

use harness_contracts::{KEY_HASH_STORE, KEY_TOOLS, ToolError, ToolSpec};
use harness_core::{Context, Result};
use harness_hash_base::HashStore;
use harness_tools::Tools;

pub struct HashlineReadPlugin;

impl harness_core::Plugin for HashlineReadPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("hashline-read")
            .injects(KEY_TOOLS)
            .injects(KEY_HASH_STORE)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS)?;
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE)?;
        tools.register(hashline_read_spec(), move |args| {
            render::hashline_read_handler(Arc::clone(&store), args)
        })?;
        Ok(())
    }
}

pub(crate) fn tool_err(message: impl Into<String>) -> ToolError {
    ToolError {
        tool: "hashline_read".to_owned(),
        message: message.into(),
    }
}

pub fn hashline_read_spec() -> ToolSpec {
    ToolSpec {
        name: "hashline_read".to_owned(),
        description: "Mmap read with 4-char xxh3 line hashes. Returns ¶path#REV:{rev} header then {line}:{hash}|{content} per line. Paginate large files with 1-based offset and limit. Use hashes with hashline_edit.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or cwd-relative file path" },
                "offset": { "type": "integer", "minimum": 1, "description": "1-based first line to render (default 1)" },
                "limit": { "type": "integer", "minimum": 1, "description": "Max lines to render (default all)" }
            },
            "required": ["path"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_TOOLS, ToolCall};
    use std::io::Write;

    fn ctx_with_plugins() -> Context {
        let ctx = Context::root();
        ctx.load(harness_tools::ToolsPlugin).unwrap();
        ctx.load(harness_hash_base::HashBasePlugin).unwrap();
        ctx.load(HashlineReadPlugin).unwrap();
        ctx
    }

    #[tokio::test]
    async fn tool_executes_through_registry() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-tool.txt", std::process::id()));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "a\nb").unwrap();
        }
        let call = ToolCall {
            id: "c1".into(),
            name: "hashline_read".into(),
            arguments: serde_json::json!({"path": path.display().to_string()}).to_string(),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.contains("#REV:"));
        assert!(out.contains("1:"));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn paginated_call_through_registry() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-toolpage.txt", std::process::id()));
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let call = ToolCall {
            id: "c2".into(),
            name: "hashline_read".into(),
            arguments: serde_json::json!({"path": path.display().to_string(), "offset": 3}).to_string(),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.contains("lines 3-3 of 3"), "got: {out}");
        assert!(out.contains("3:"), "got: {out}");
        assert!(!out.contains("1:"), "got: {out}");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn missing_file_is_clean_error() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let call = ToolCall {
            id: "c3".into(),
            name: "hashline_read".into(),
            arguments: r#"{"path":"/definitely/not/here-xyz.txt"}"#.to_string(),
        };
        let err = tools.execute("a", "s", 1, call).await.unwrap_err();
        assert_eq!(err.tool, "hashline_read");
    }
}
