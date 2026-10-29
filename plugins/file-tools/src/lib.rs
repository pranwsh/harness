//! Bounded reads and snapshot-addressed transactional edits.
mod edit;
mod read;
mod syntax;
mod text;

use harness_contracts::{
    KEY_HASH_STORE, KEY_TOOL_REGISTRY, ToolError, ToolRegistryHandle, ToolSpec,
};
use harness_core::{Context, Result};
use harness_hash_base::HashStore;
use std::sync::Arc;

pub struct FileToolsPlugin;

impl harness_core::Plugin for FileToolsPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("file-tools")
            .injects(KEY_TOOL_REGISTRY)
            .injects(KEY_HASH_STORE)
    }
    fn build(&self, ctx: Context) -> Result<()> {
        let registry: Arc<ToolRegistryHandle> = ctx.inject_key(KEY_TOOL_REGISTRY)?;
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE)?;
        let read_store = store.clone();
        registry
            .register(
                read_spec(),
                Box::new(move |arguments| {
                    let store = read_store.clone();
                    Box::pin(async move {
                        tokio::task::spawn_blocking(move || {
                            let args = serde_json::from_str(&arguments)
                                .map_err(|e| error("read", format!("invalid arguments: {e}")))?;
                            read::run(&store, args).map_err(|e| error("read", e))
                        })
                        .await
                        .map_err(|e| error("read", format!("task failed: {e}")))?
                    })
                }),
            )
            .map_err(|e| harness_core::Error::ServiceConflict {
                key: "read".into(),
                provider: e,
            })?;
        let editor = Arc::new(edit::Editor::default());
        registry
            .register(
                edit_spec(),
                Box::new(move |arguments| {
                    let store = store.clone();
                    let editor = editor.clone();
                    Box::pin(async move {
                        tokio::task::spawn_blocking(move || {
                            let args = serde_json::from_str(&arguments)
                                .map_err(|e| error("edit", format!("invalid arguments: {e}")))?;
                            editor.run(&store, args).map_err(|e| error("edit", e))
                        })
                        .await
                        .map_err(|e| error("edit", format!("task failed: {e}")))?
                    })
                }),
            )
            .map_err(|e| harness_core::Error::ServiceConflict {
                key: "edit".into(),
                provider: e,
            })?;
        Ok(())
    }
}
fn error(tool: &str, message: String) -> ToolError {
    ToolError {
        tool: tool.into(),
        message,
    }
}

pub fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read path or paths. Select ranges [{start,end}] (inclusive 1-based), symbol, around {pattern|anchor,before,after}, search {regex,glob,context,max_matches,output:matches|files|count}, or outline:true. Default first 50 lines. Anchors default true: line:hash|text; use {line,hash} with edit. Each file has an exact-content version. max_tokens defaults 4096 (cl100k_base), max_line_width 300; overflow is explicit. Symbols/parse support Rust, Python, JS, TS/TSX, JSON. Search directories recursively without following symlinks.".into(),
        parameters: serde_json::to_value(schemars::schema_for!(read::Args)).expect("read schema"),
    }
}
pub fn edit_spec() -> ToolSpec {
    ToolSpec {
        name: "edit".into(),
        description: "Apply edits:[{path,expected_version?,op,...}] against one snapshot per file. Ops: replace/delete {start:{line,hash},end:{line,hash},text?}; insert_before/insert_after {anchor,text}; replace_text {old,new,occurrence? (1-based),all?}; create {text}; move {to}; delete_file. All validate before writes; bottom-up edits avoid line drift. dry_run shows diff without writing. Syntax validation defaults on; auto_rollback defaults true (reject invalid syntax before publication). Unsupported parsers are reported. Conflicts return current anchors; success returns diff, fresh anchors, version, edit_id. undo:edit_id restores a process-local transaction only if files still match. Multi-file failures roll back; publication is not crash-atomic. Whitespace fallback is reported. max_tokens defaults 4096.".into(),
        parameters: serde_json::to_value(schemars::schema_for!(edit::Args)).expect("edit schema"),
    }
}

#[cfg(test)]
mod tests;
