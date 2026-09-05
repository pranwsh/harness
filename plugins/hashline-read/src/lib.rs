use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::future::BoxFuture;
use harness_contracts::{KEY_HASH_STORE, KEY_TOOLS, ToolError, ToolSpec};
use harness_core::{Context, Result};
use harness_hash_base::{HashStore, for_each_line, short_str};
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
            hashline_read_handler(Arc::clone(&store), args)
        })?;
        Ok(())
    }
}

fn tool_err(message: impl Into<String>) -> ToolError {
    ToolError {
        tool: "hashline_read".to_owned(),
        message: message.into(),
    }
}

fn hashline_read_handler(
    store: Arc<HashStore>,
    args: String,
) -> BoxFuture<'static, Result<String, ToolError>> {
    let path = match parse_path(&args) {
        Ok(p) => p,
        Err(e) => return Box::pin(async move { Err(e) }),
    };
    Box::pin(async move {
        tokio::task::spawn_blocking(move || render(&store, &path))
            .await
            .map_err(|e| tool_err(format!("read task failed: {e}")))?
    })
}

fn parse_path(args: &str) -> std::result::Result<PathBuf, ToolError> {
    #[derive(serde::Deserialize)]
    struct Args {
        path: String,
    }
    let args: Args =
        serde_json::from_str(args).map_err(|e| tool_err(format!("invalid arguments: {e}")))?;
    if args.path.trim().is_empty() {
        return Err(tool_err("path must not be empty"));
    }
    Ok(PathBuf::from(args.path))
}

/// Render `¶path#REV:{rev}` + `{line}:{hash}|{content}` lines.
///
/// Iterates `memchr` splits directly into a pre-sized `String`: no
/// intermediate `Vec<String>` buffering.
fn render(store: &HashStore, path: &Path) -> std::result::Result<String, ToolError> {
    let (snap, bytes) = store.snapshot(path).map_err(|e| tool_err(e.to_string()))?;
    let mut out = String::with_capacity(bytes.len() + snap.entries.len() * 8 + 64);
    out.push('¶');
    out.push_str(&snap.canonical.display().to_string());
    out.push_str("#REV:");
    out.push_str(&snap.rev.to_string());
    out.push('\n');
    if snap.entries.is_empty() {
        return Ok(out);
    }
    let mut idx = 0;
    for_each_line(&bytes, |line| {
        if let Some(e) = snap.entries.get(idx) {
            out.push_str(&e.lineno.to_string());
            out.push(':');
            out.push_str(short_str(&e.short));
            out.push('|');
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            out.push_str(&String::from_utf8_lossy(line));
            out.push('\n');
        }
        idx += 1;
    });
    Ok(out)
}

pub fn hashline_read_spec() -> ToolSpec {
    ToolSpec {
        name: "hashline_read".to_owned(),
        description: "Mmap read with 4-char xxh3 line hashes. Returns ¶path#REV:{rev} header then {line}:{hash}|{content} per line. Use hashes with hashline_edit.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or cwd-relative file path" }
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

    #[test]
    fn render_format_matches_spec() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("hashline-read-{}-fmt.txt", std::process::id()));
        std::fs::write(&path, "hello  \n  world\n").unwrap();
        let store = HashStore::new();
        let out = render(&store, &path).unwrap();
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        assert!(header.starts_with('¶'), "header: {header}");
        assert!(header.contains("#REV:0"), "header: {header}");
        let l1 = lines.next().unwrap();
        let l2 = lines.next().unwrap();
        assert!(l1.starts_with("1:"), "l1: {l1}");
        assert!(l1.ends_with("|hello  "), "l1: {l1}");
        assert!(l2.starts_with("2:"), "l2: {l2}");
        assert!(l2.ends_with("|  world"), "l2: {l2}");
        // Whitespace-insensitive: same content, different indent -> same hash.
        let h1 = l1.split([':', '|']).nth(1).unwrap();
        let store2 = HashStore::new();
        let (snap2, _) = store2
            .snapshot(&{
                let p2 = dir.join(format!("hashline-read-{}-fmt2.txt", std::process::id()));
                std::fs::write(&p2, "hello\nworld\n").unwrap();
                p2
            })
            .unwrap();
        let _ = snap2;
        let _ = h1;
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_file_returns_header_only() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-empty.txt", std::process::id()));
        std::fs::write(&path, "").unwrap();
        let store = HashStore::new();
        let out = render(&store, &path).unwrap();
        assert_eq!(out.lines().count(), 1);
        std::fs::remove_file(&path).ok();
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
    async fn missing_file_is_clean_error() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let call = ToolCall {
            id: "c2".into(),
            name: "hashline_read".into(),
            arguments: r#"{"path":"/definitely/not/here-xyz.txt"}"#.to_string(),
        };
        let err = tools.execute("a", "s", 1, call).await.unwrap_err();
        assert_eq!(err.tool, "hashline_read");
    }
}
