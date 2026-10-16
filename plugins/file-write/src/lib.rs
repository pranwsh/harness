//! `file_write`: whole-file write without a shell heredoc.
//!
//! Replaces `cat > file <<'EOF'` for agent-driven file creation: the content
//! travels as a JSON string (no quoting, no `;` surprises), the write is
//! atomic (sibling temp + rename, same helper as `hashline_edit`), and the
//! in-memory `REV` advances so later `hashline_read`/`hashline_edit` calls
//! stay consistent.
//!
//! Two modes: `create` (default; fails if the file exists, parents must
//! exist) and `overwrite` (fails if the file is missing, preserves the
//! existing mode best-effort).

use std::sync::Arc;

use futures::future::BoxFuture;
use harness_contracts::{
    ToolRegistryHandle, KEY_HASH_STORE, KEY_TOOL_REGISTRY, ToolError, ToolSpec,
};
use harness_core::{Context, Result};
use harness_hash_base::{HashError, HashStore};

pub struct FileWritePlugin;

impl harness_core::Plugin for FileWritePlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("file-write")
            .injects(KEY_TOOL_REGISTRY)
            .injects(KEY_HASH_STORE)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let registry: Arc<ToolRegistryHandle> = ctx.inject_key(KEY_TOOL_REGISTRY)?;
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE)?;
        registry
            .register(file_write_spec(), Box::new(move |args| {
                file_write_handler(Arc::clone(&store), args)
            }))
            .map_err(|e| harness_core::Error::ServiceConflict {
                key: "file_write".into(),
                provider: e,
            })?;
        Ok(())
    }
}

pub(crate) fn tool_err(message: impl Into<String>) -> ToolError {
    ToolError {
        tool: "file_write".to_owned(),
        message: message.into(),
    }
}

pub fn file_write_spec() -> ToolSpec {
    ToolSpec {
        name: "file_write".to_owned(),
        description: "Write a whole file atomically (no shell heredoc). Args {path, text, mode: create|overwrite (default create)}. create fails if the file exists; overwrite fails if missing (preserves mode). Parents must exist. Bumps REV; returns OK path#REV:n.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or cwd-relative file path" },
                "text": { "type": "string", "description": "Full file content" },
                "mode": { "type": "string", "enum": ["create", "overwrite"], "description": "create (default): fail if exists; overwrite: fail if missing" }
            },
            "required": ["path", "text"]
        }),
    }
}

fn file_write_handler(
    store: Arc<HashStore>,
    args: String,
) -> BoxFuture<'static, harness_core::Result<String, ToolError>> {
    match parse_args(&args) {
        Ok(a) => Box::pin(async move {
            tokio::task::spawn_blocking(move || write(&store, &a))
                .await
                .map_err(|e| tool_err(format!("write task failed: {e}")))?
        }),
        Err(e) => Box::pin(async move { Err(e) }),
    }
}

struct WriteArgs {
    path: std::path::PathBuf,
    text: String,
    overwrite: bool,
}

fn parse_args(args: &str) -> std::result::Result<WriteArgs, ToolError> {
    #[derive(serde::Deserialize)]
    struct Raw {
        path: String,
        text: String,
        #[serde(default)]
        mode: Option<String>,
    }
    let raw: Raw = serde_json::from_str(args)
        .map_err(|e| tool_err(format!("invalid arguments: {e}")))?;
    if raw.path.trim().is_empty() {
        return Err(tool_err("path must not be empty"));
    }
    let overwrite = match raw.mode.as_deref().unwrap_or("create") {
        "create" => false,
        "overwrite" => true,
        other => return Err(tool_err(format!("mode must be create|overwrite, got {other:?}"))),
    };
    Ok(WriteArgs {
        path: std::path::PathBuf::from(raw.path),
        text: raw.text,
        overwrite,
    })
}

fn hash_to_tool(e: HashError) -> ToolError {
    tool_err(e.to_string())
}

/// Whole-file write under the same locks as `hashline_edit`: in-process edit
/// mutex plus the per-file lockfile, directory guard, then atomic persist.
fn write(store: &HashStore, a: &WriteArgs) -> std::result::Result<String, ToolError> {
    let _edit = store.lock_edit();
    let canonical = store.canonicalize(&a.path);
    let _fs = store
        .exclusive_file_lock(&canonical)
        .map_err(hash_to_tool)?;

    if std::fs::metadata(&canonical)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(tool_err(format!(
            "{} is a directory, not a file",
            a.path.display()
        )));
    }
    let exists = canonical.exists();
    if a.overwrite && !exists {
        return Err(tool_err(format!(
            "file does not exist: {} (use mode create)",
            canonical.display()
        )));
    }
    if !a.overwrite && exists {
        return Err(tool_err(format!(
            "file already exists: {} (use mode overwrite)",
            canonical.display()
        )));
    }

    let parent = canonical
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| tool_err("path has no parent"))?;
    if !parent.is_dir() {
        return Err(tool_err(format!(
            "parent directory {} does not exist",
            parent.display()
        )));
    }

    // Preserve mode on overwrite (best-effort); creates take umask default.
    let perm = if a.overwrite {
        std::fs::metadata(&canonical).ok().map(|m| m.permissions())
    } else {
        None
    };
    let buf = a.text.as_bytes();
    store
        .atomic_write(&canonical, parent, buf, perm, "file_write")
        .map_err(hash_to_tool)?;
    store
        .commit_write(&canonical, "file_write", 1)
        .map_err(hash_to_tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_TOOLS, ToolCall};
    use harness_tools::Tools;

    fn ctx_with_plugins() -> Context {
        let ctx = Context::root();
        ctx.load(harness_tools::ToolsPlugin).unwrap();
        ctx.load(harness_hash_base::HashBasePlugin).unwrap();
        ctx.load(FileWritePlugin).unwrap();
        ctx
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("file-write-{}-{tag}.txt", std::process::id()))
    }

    #[test]
    fn mode_defaults_to_create_and_rejects_unknown() {
        let a = parse_args(r#"{"path":"f","text":"x"}"#).unwrap();
        assert!(!a.overwrite);
        assert!(parse_args(r#"{"path":"f","text":"x","mode":"overwrite"}"#)
            .unwrap()
            .overwrite);
        assert!(parse_args(r#"{"path":"f","text":"x","mode":"nuke"}"#).is_err());
        assert!(parse_args(r#"{"path":"","text":"x"}"#).is_err());
    }

    #[test]
    fn create_then_overwrite_round_trip() {
        let store = HashStore::new();
        let p = tmp("roundtrip");
        std::fs::remove_file(&p).ok();
        let out = write(
            &store,
            &WriteArgs {
                path: p.clone(),
                text: "a\nb 'quoted' $x;\n".into(),
                overwrite: false,
            },
        )
        .unwrap();
        assert!(out.contains("#REV:1"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "a\nb 'quoted' $x;\n"
        );
        // create twice fails; content untouched.
        let err = write(
            &store,
            &WriteArgs {
                path: p.clone(),
                text: "nope".into(),
                overwrite: false,
            },
        )
        .unwrap_err();
        assert!(err.message.contains("already exists"), "got: {}", err.message);
        // overwrite works and bumps rev.
        let out = write(
            &store,
            &WriteArgs {
                path: p.clone(),
                text: "c\n".into(),
                overwrite: true,
            },
        )
        .unwrap();
        assert!(out.contains("#REV:2"), "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "c\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn overwrite_missing_and_bad_parent_are_clean_errors() {
        let store = HashStore::new();
        let p = tmp("missing");
        std::fs::remove_file(&p).ok();
        let err = write(
            &store,
            &WriteArgs {
                path: p.clone(),
                text: "x".into(),
                overwrite: true,
            },
        )
        .unwrap_err();
        assert!(err.message.contains("does not exist"), "got: {}", err.message);

        let orphan = std::path::PathBuf::from(format!(
            "{}/no-such-dir-{}/f.txt",
            std::env::temp_dir().display(),
            std::process::id()
        ));
        let err = write(
            &store,
            &WriteArgs {
                path: orphan,
                text: "x".into(),
                overwrite: false,
            },
        )
        .unwrap_err();
        assert!(err.message.contains("parent directory"), "got: {}", err.message);
    }

    #[test]
    fn directory_is_a_clean_error() {
        let store = HashStore::new();
        let err = write(
            &store,
            &WriteArgs {
                path: std::env::temp_dir(),
                text: "x".into(),
                overwrite: false,
            },
        )
        .unwrap_err();
        assert!(err.message.contains("is a directory"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn tool_executes_through_registry() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let path = tmp("registry");
        std::fs::remove_file(&path).ok();
        let call = ToolCall {
            id: "c1".into(),
            name: "file_write".into(),
            arguments: serde_json::json!({
                "path": path.display().to_string(),
                "text": "echo 'no shell needed; $safe'\n",
            })
            .to_string(),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.contains("#REV:"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "echo 'no shell needed; $safe'\n"
        );
        std::fs::remove_file(&path).ok();
    }
}
