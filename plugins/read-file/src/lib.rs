use std::sync::Arc;

use futures::future::BoxFuture;
use harness_contracts::{KEY_TOOLS, ToolError, ToolSpec};
use harness_core::{Context, Result};
use harness_tools::Tools;

pub struct ReadFilePlugin;

impl harness_core::Plugin for ReadFilePlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("read-file").injects(KEY_TOOLS)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS)?;
        tools.register(read_file_spec(), read_file_handler)?;
        Ok(())
    }
}

fn tool_err(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError {
        tool: tool.to_owned(),
        message: message.into(),
    }
}

fn read_file_handler(args: String) -> BoxFuture<'static, Result<String, ToolError>> {
    let path = match parse_path(&args) {
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
    use harness_core::Context;

    #[tokio::test]
    async fn read_file_tool_reads_disk() {
        let ctx = Context::root();
        ctx.load(harness_tools::ToolsPlugin).unwrap();
        ctx.load(ReadFilePlugin).unwrap();

        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let call = ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"Cargo.toml"}"#.into(),
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.contains("harness-read-file"));
    }
}
