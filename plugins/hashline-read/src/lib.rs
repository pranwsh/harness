//! `hashline_read`: mmap-backed file read with stable per-line hashes.
//!
//! Default output is edit-ready: a `¶path#REV:{rev} lines=N` header (plus
//! `matches=`/`window=a-b`/`next=K` when they apply) then
//! `{line}:{hash}|{content}` rows usable directly by `hashline_edit`. Pass
//! `hashes: false` for the quieter `{line}: {content}` form. Line numbers are
//! absolute in every mode, so a paginated hashed read is still editable.

mod args;
mod render;

use std::sync::Arc;

use harness_contracts::{ToolRegistryHandle, KEY_HASH_STORE, KEY_TOOL_REGISTRY, ToolError, ToolSpec};
use harness_core::{Context, Result};
use harness_hash_base::HashStore;

pub struct HashlineReadPlugin;

impl harness_core::Plugin for HashlineReadPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("hashline-read")
            .injects(KEY_TOOL_REGISTRY)
            .injects(KEY_HASH_STORE)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let registry: Arc<ToolRegistryHandle> = ctx.inject_key(KEY_TOOL_REGISTRY)?;
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE)?;
        registry
            .register(hashline_read_spec(), Box::new(move |args| {
                render::hashline_read_handler(Arc::clone(&store), args)
            }))
            .map_err(|e| harness_core::Error::ServiceConflict {
                key: "hashline_read".into(),
                provider: e,
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
        description: "Read a file (default: first 50 lines, edit-ready `line:hash|content` rows). \
            Header `¶path#REV:{rev} lines=N` plus `matches=N`/`window=a-b`/`next=K` when they apply. \
            Options: offset/limit (1-based; limit<=0 = to EOF), hashes=false for quiet `line: content` rows, \
            grep=SUBSTR (+ignore_case) to filter, context=K to include K lines around each hit, \
            if_rev_match=REV to elide the body when unchanged. \
            Hashes are what hashline_edit anchors on; a bare read is read-only. Errors clearly on directories."
            .to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or cwd-relative file path" },
                "offset": { "type": "integer", "minimum": 1, "description": "1-based first line to render (default 1)" },
                "limit": { "type": "integer", "description": "Max lines to render (default 50; <=0 means to EOF)" },
                "hashes": { "type": "boolean", "description": "Emit edit-ready `line:hash|content` rows (default true; false for quiet rows)" },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive grep (default false)" },
                "context": { "type": "integer", "minimum": 0, "description": "Lines of context around each grep match (default 0)" },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive grep (default false)" },
                "if_rev_match": { "type": "integer", "minimum": 0, "description": "Elide the body when the file rev equals this" }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_TOOLS, ToolCall};
    use harness_tools::Tools;
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
        assert!(out.contains("lines=3"), "got: {out}");
        assert!(out.contains("window=3-3"), "got: {out}");
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

    #[tokio::test]
    async fn directory_lists_through_registry() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let call = ToolCall {
            id: "c4".into(),
            name: "hashline_read".into(),
            arguments: serde_json::json!({"path": std::env::temp_dir().display().to_string()})
                .to_string(),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.contains("directory"), "got: {out}");
    }
}
