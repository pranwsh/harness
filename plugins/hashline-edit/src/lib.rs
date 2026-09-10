//! `hashline_edit`: batch edits addressed by `hashline_read` line hashes.
//!
//! All hashes resolve against one pre-write snapshot and every op validates
//! before anything touches disk; the write itself is atomic (sibling temp
//! file + rename) and bumps the in-memory `REV`. See [`ops`] for the op
//! shapes and [`apply`] for the transaction.

mod apply;
mod ops;

use std::sync::Arc;

use harness_contracts::{KEY_HASH_STORE, KEY_TOOLS, ToolError, ToolSpec};
use harness_core::{Context, Result};
use harness_hash_base::HashStore;
use harness_tools::Tools;

pub struct HashlineEditPlugin;

impl harness_core::Plugin for HashlineEditPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("hashline-edit")
            .injects(KEY_TOOLS)
            .injects(KEY_HASH_STORE)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS)?;
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE)?;
        tools.register(hashline_edit_spec(), move |args| {
            apply::hashline_edit_handler(Arc::clone(&store), args)
        })?;
        Ok(())
    }
}

pub(crate) fn tool_err(message: impl Into<String>) -> ToolError {
    ToolError {
        tool: "hashline_edit".to_owned(),
        message: message.into(),
    }
}

pub fn hashline_edit_spec() -> ToolSpec {
    ToolSpec {
        name: "hashline_edit".to_owned(),
        description: "Batch edit by 4-char line hashes. Args {path, rev, ops:[{op:set,hash,content}|{op:insert,after_hash,lines}|{op:delete,start_hash,end_hash}|{op:replace,start_hash,end_hash,lines}|{op:create,lines}]}. after_hash HEAD prepends; create makes a new file (rev 0, fails if it exists). All hashes validated first, applied bottom-up atomically; rev bumps on success. Re-read on stale/unknown hash.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "rev": { "type": "integer", "minimum": 0 },
                "ops": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": { "type": "string", "enum": ["set", "insert", "delete", "replace", "create"] },
                            "hash": { "type": "string" },
                            "content": { "type": "string" },
                            "after_hash": { "type": "string" },
                            "lines": { "type": "array", "items": { "type": "string" } },
                            "start_hash": { "type": "string" },
                            "end_hash": { "type": "string" }
                        },
                        "required": ["op"]
                    }
                }
            },
            "required": ["path", "rev", "ops"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_HASH_STORE, KEY_TOOLS, ToolCall};
    use harness_hash_base::{HashBasePlugin, short_str};
    use harness_tools::ToolsPlugin;
    use std::path::{Path, PathBuf};

    fn ctx_with_plugins() -> Context {
        let ctx = Context::root();
        ctx.load(ToolsPlugin).unwrap();
        ctx.load(HashBasePlugin).unwrap();
        ctx.load(crate::HashlineEditPlugin).unwrap();
        ctx
    }

    fn tmpfile(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hashline-edit-reg-{}-{tag}.txt", std::process::id()))
    }

    fn read_hashes(store: &HashStore, path: &Path) -> Vec<String> {
        let (snap, _) = store.snapshot(path).unwrap();
        snap.entries
            .iter()
            .map(|e| short_str(&e.short).to_owned())
            .collect()
    }

    fn edit_json(path: &Path, rev: u64, ops: serde_json::Value) -> String {
        serde_json::json!({"path": path.display().to_string(), "rev": rev, "ops": ops}).to_string()
    }

    #[tokio::test]
    async fn tool_executes_through_registry() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE).unwrap();
        let p = tmpfile("reg");
        std::fs::write(&p, "x\ny\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = edit_json(
            &p,
            0,
            serde_json::json!([{"op":"set","hash":h[0],"content":"X"}]),
        );
        let call = ToolCall {
            id: "c1".into(),
            name: "hashline_edit".into(),
            arguments: args,
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.starts_with("OK "));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "X\ny\n");
        std::fs::remove_file(&p).ok();
    }

    #[tokio::test]
    async fn create_and_replace_round_trip_through_registry() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE).unwrap();
        let p = tmpfile("reg-create");
        std::fs::remove_file(&p).ok();

        // create validates the {"op":"create","lines":[...]} JSON shape.
        let call = ToolCall {
            id: "c2".into(),
            name: "hashline_edit".into(),
            arguments: edit_json(&p, 0, serde_json::json!([{"op":"create","lines":["a","b","c"]}])),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.contains("#REV:1"), "got: {out}");

        // replace validates the multi-line shape against live hashes.
        let h = read_hashes(&store, &p);
        let call = ToolCall {
            id: "c3".into(),
            name: "hashline_edit".into(),
            arguments: edit_json(
                &p,
                1,
                serde_json::json!([{"op":"replace","start_hash":h[0],"end_hash":h[1],"lines":["A"]}]),
            ),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.contains("#REV:2"), "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "A\nc\n");
        std::fs::remove_file(&p).ok();
    }
}
