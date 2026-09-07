//! Headless shell tool plugin: bounded, non-interactive subprocesses.
//!
//! Four tools over one [`ShellService`]:
//! `shell_exec` (sync), `shell_start` / `shell_poll` / `shell_stop`
//! (background). No policy guardrails live here: any executable runs with
//! the agent's environment. Output is tail-truncated and labeled; background
//! jobs can never stall the loop (`start` returns an id immediately, `poll`
//! is a non-blocking snapshot).
//!
//! Policy (allowlist, workdir scoping, env filtering) belongs in a future
//! guardrail plugin via the `tool.approval` waterfall on [`harness_tools`],
//! which can rewrite or deny calls before they reach these handlers.
//!
//! Without that plugin this is unconstrained execution: run only trusted
//! work, preferably in a container.

mod env;
mod exec;
mod jobs;
mod output;

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use harness_config::{AppConfig, ShellConfig};
use harness_contracts::{KEY_CONFIG, KEY_SHELL_SERVICE, KEY_TOOLS, ToolError, ToolSpec};
use harness_core::{Context, Result};

pub use jobs::JobLimits;
pub use output::{Tail, format_result, format_result_full, tail_truncate};

use exec::resolve_request;
use jobs::{JobManager, validate_env_keys};

const NO_POLICY_NOTE: &str = "No policy guardrails: any executable runs. A future guardrail plugin may deny via tool.approval. Run trusted work only, preferably in a container.";

fn tool_err(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError {
        tool: tool.to_owned(),
        message: message.into(),
    }
}

/// Runtime service behind the four shell tools. Clone via `Arc`.
pub struct ShellService {
    cfg: ShellConfig,
    jobs: JobManager,
}

impl ShellService {
    pub fn new(cfg: ShellConfig) -> Self {
        let limits = JobLimits::from_config(&cfg);
        ShellService {
            cfg,
            jobs: JobManager::new(limits),
        }
    }

    pub fn config(&self) -> &ShellConfig {
        &self.cfg
    }

    pub fn job_count(&self) -> usize {
        self.jobs.job_count()
    }

    /// Graceful shutdown: kill all background jobs (harness exit / tests).
    pub async fn shutdown(&self) {
        self.jobs.shutdown().await;
    }

    /// Argument-shape validation only (no policy). Returns the executable.
    fn check_common(
        &self,
        tool: &str,
        argv: &[String],
        env: &Option<HashMap<String, String>>,
    ) -> std::result::Result<String, ToolError> {
        if argv.is_empty() {
            return Err(tool_err(tool, "command must be a non-empty array"));
        }
        if argv.len() > 128 {
            return Err(tool_err(tool, "command has too many arguments (max 128)"));
        }
        if argv.iter().map(String::len).sum::<usize>() > 64 * 1024 {
            return Err(tool_err(tool, "command is too long (max 64KiB total)"));
        }
        validate_env_keys(env.as_ref()).map_err(|e| tool_err(tool, e))?;
        let exe = argv[0].trim().to_owned();
        if exe.is_empty() {
            return Err(tool_err(tool, "empty executable"));
        }
        Ok(exe)
    }

    /// Synchronous execution. Non-zero exits are `Ok` (labeled output);
    /// spawn failures are `Err`.
    pub async fn exec_sync(
        &self,
        tool: &'static str,
        argv: Vec<String>,
        workdir: Option<String>,
        timeout_ms: Option<u64>,
        env: Option<HashMap<String, String>>,
    ) -> std::result::Result<String, ToolError> {
        let exe = self.check_common(tool, &argv, &env)?;
        let req = resolve_request(&self.cfg, exe, argv, workdir, timeout_ms, env)
            .map_err(|e| tool_err(tool, e))?;
        exec::run_once(&self.cfg, req)
            .await
            .map_err(|e| tool_err(tool, e))
    }

    /// Launch a background job; returns immediately with the job id.
    pub async fn start(
        &self,
        tool: &'static str,
        argv: Vec<String>,
        workdir: Option<String>,
        timeout_ms: Option<u64>,
        env: Option<HashMap<String, String>>,
    ) -> std::result::Result<String, ToolError> {
        let exe = self.check_common(tool, &argv, &env)?;
        let id = self
            .jobs
            .start(&self.cfg, exe, argv, workdir, timeout_ms, env)
            .await
            .map_err(|e| tool_err(tool, e))?;
        Ok(format!(
            "started {id}\nuse shell_poll with job_id \"{id}\" to read output"
        ))
    }

    /// Non-blocking output snapshot for a job.
    pub async fn poll(
        &self,
        tool: &'static str,
        job_id: &str,
        tail_bytes: Option<usize>,
    ) -> std::result::Result<String, ToolError> {
        if job_id.trim().is_empty() {
            return Err(tool_err(tool, "job_id must not be empty"));
        }
        self.jobs
            .poll(job_id, tail_bytes)
            .await
            .map_err(|e| tool_err(tool, e))
    }

    /// Signal a job to stop; bounded ~2s reap wait.
    pub async fn stop(
        &self,
        tool: &'static str,
        job_id: &str,
    ) -> std::result::Result<String, ToolError> {
        if job_id.trim().is_empty() {
            return Err(tool_err(tool, "job_id must not be empty"));
        }
        self.jobs.stop(job_id).await.map_err(|e| tool_err(tool, e))
    }
}

// ---------------------------------------------------------------------------
// Tool argument envelopes
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct ExecArgs {
    command: Vec<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
}

#[derive(Debug, serde::Deserialize)]
struct PollArgs {
    job_id: String,
    #[serde(default)]
    tail_bytes: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
struct StopArgs {
    job_id: String,
}

fn parse_exec_args(tool: &'static str, args: &str) -> std::result::Result<ExecArgs, ToolError> {
    serde_json::from_str(args).map_err(|e| tool_err(tool, format!("invalid arguments: {e}")))
}

// ---------------------------------------------------------------------------
// Handlers (each: parse -> service call; policy lives in tool.approval)
// ---------------------------------------------------------------------------

fn shell_exec_handler(
    svc: Arc<ShellService>,
    args: String,
) -> BoxFuture<'static, Result<String, ToolError>> {
    Box::pin(async move {
        let a = parse_exec_args("shell_exec", &args)?;
        svc.exec_sync("shell_exec", a.command, a.workdir, a.timeout_ms, a.env)
            .await
    })
}

fn shell_start_handler(
    svc: Arc<ShellService>,
    args: String,
) -> BoxFuture<'static, Result<String, ToolError>> {
    Box::pin(async move {
        let a = parse_exec_args("shell_start", &args)?;
        svc.start("shell_start", a.command, a.workdir, a.timeout_ms, a.env)
            .await
    })
}

fn shell_poll_handler(
    svc: Arc<ShellService>,
    args: String,
) -> BoxFuture<'static, Result<String, ToolError>> {
    Box::pin(async move {
        let a: PollArgs = serde_json::from_str(&args)
            .map_err(|e| tool_err("shell_poll", format!("invalid arguments: {e}")))?;
        svc.poll("shell_poll", &a.job_id, a.tail_bytes).await
    })
}

fn shell_stop_handler(
    svc: Arc<ShellService>,
    args: String,
) -> BoxFuture<'static, Result<String, ToolError>> {
    Box::pin(async move {
        let a: StopArgs = serde_json::from_str(&args)
            .map_err(|e| tool_err("shell_stop", format!("invalid arguments: {e}")))?;
        svc.stop("shell_stop", &a.job_id).await
    })
}

// ---------------------------------------------------------------------------
// Tool specs
// ---------------------------------------------------------------------------

fn exec_params() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": { "type": "array", "items": { "type": "string" }, "description": "argv array, e.g. [\"git\", \"status\"]. Runs directly, no shell." },
            "workdir": { "type": "string", "description": "Working directory (must exist, default cwd)" },
            "timeout_ms": { "type": "integer", "minimum": 1, "description": "Timeout override, clamped to max_timeout_ms" },
            "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "If given, subprocess gets ONLY these vars (clean env)" }
        },
        "required": ["command"]
    })
}

pub fn shell_exec_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_exec".to_owned(),
        description: format!(
            "Run a command synchronously and return labeled tail-truncated [stdout]/[stderr]. {NO_POLICY_NOTE}"
        ),
        parameters: exec_params(),
    }
}

pub fn shell_start_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_start".to_owned(),
        description: format!(
            "Start a long-running command in the background; returns a job id immediately (never blocks). Poll with shell_poll, end with shell_stop. {NO_POLICY_NOTE}"
        ),
        parameters: exec_params(),
    }
}

pub fn shell_poll_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_poll".to_owned(),
        description: "Non-blocking snapshot of a background job: running state plus tail-truncated [stdout]/[stderr].".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "Job id from shell_start (e.g. sh-1)" },
                "tail_bytes": { "type": "integer", "minimum": 1, "description": "Tail bytes per stream" }
            },
            "required": ["job_id"]
        }),
    }
}

pub fn shell_stop_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_stop".to_owned(),
        description:
            "Stop a background job (kill + reap, bounded wait) and return its final output tail."
                .to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "Job id from shell_start" }
            },
            "required": ["job_id"]
        }),
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Headless shell plugin. Reads `[shell]` bounds from `config.app` via DI.
///
/// ```rust,no_run
/// # use harness_shell::ShellPlugin;
/// let plugin = ShellPlugin;
/// ```
#[derive(Default, Debug, Clone, Copy)]
pub struct ShellPlugin;

impl harness_core::Plugin for ShellPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("shell")
            .provides(KEY_SHELL_SERVICE)
            .injects(KEY_TOOLS)
            .injects(KEY_CONFIG)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        use harness_tools::Tools;
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS)?;
        let config: Arc<AppConfig> = ctx.inject_key(KEY_CONFIG)?;
        let svc = Arc::new(ShellService::new(config.shell.clone()));
        tools.register(shell_exec_spec(), {
            let svc = Arc::clone(&svc);
            move |args| shell_exec_handler(Arc::clone(&svc), args)
        })?;
        tools.register(shell_start_spec(), {
            let svc = Arc::clone(&svc);
            move |args| shell_start_handler(Arc::clone(&svc), args)
        })?;
        tools.register(shell_poll_spec(), {
            let svc = Arc::clone(&svc);
            move |args| shell_poll_handler(Arc::clone(&svc), args)
        })?;
        tools.register(shell_stop_spec(), {
            let svc = Arc::clone(&svc);
            move |args| shell_stop_handler(Arc::clone(&svc), args)
        })?;
        // Keep the service alive for the plugin lifetime; dropping it aborts
        // background jobs via JobManager::drop. Stored under `KEY_SHELL_SERVICE`
        // owned by this plugin so unload cleans it up.
        ctx.provide_key(KEY_SHELL_SERVICE, svc);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_TOOLS, ToolCall};
    use harness_tools::{Tools, ToolsPlugin};

    const TEST_CONFIG_TOML: &str = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"
"#;

    fn ctx_with_shell() -> (Context, Arc<Tools>, Arc<ShellService>) {
        let ctx = Context::root();
        ctx.load(
            harness_config::ConfigPlugin::from_toml(TEST_CONFIG_TOML).unwrap(),
        )
        .unwrap();
        ctx.load(ToolsPlugin).unwrap();
        ctx.load(ShellPlugin).unwrap();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let svc: Arc<ShellService> = ctx.inject_key(KEY_SHELL_SERVICE).unwrap();
        (ctx, tools, svc)
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn exec_tool_runs_any_command() {
        let (_ctx, tools, _svc) = ctx_with_shell();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_exec", serde_json::json!({"command": ["echo", "hi"]})),
            )
            .await
            .unwrap();
        assert!(out.contains("hi"), "got: {out}");
        assert!(out.contains("[stdout]"));
    }

    #[tokio::test]
    async fn exec_tool_rejects_empty_command() {
        let (_ctx, tools, _svc) = ctx_with_shell();
        let err = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_exec", serde_json::json!({"command": []})),
            )
            .await
            .unwrap_err();
        assert_eq!(err.tool, "shell_exec");
    }

    #[tokio::test]
    async fn background_lifecycle_through_registry() {
        let (_ctx, tools, svc) = ctx_with_shell();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_start",
                    serde_json::json!({"command": ["python3", "-c", "import time; time.sleep(30)"]}),
                ),
            )
            .await
            .unwrap();
        assert!(out.contains("sh-"), "got: {out}");
        let id: String = out
            .split_whitespace()
            .find(|w| w.starts_with("sh-"))
            .unwrap()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_owned();
        let poll = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_poll", serde_json::json!({"job_id": id})),
            )
            .await
            .unwrap();
        assert!(poll.contains("running=true"), "got: {poll}");
        let stopped = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_stop", serde_json::json!({"job_id": id})),
            )
            .await
            .unwrap();
        assert!(
            stopped.contains("stopped") || stopped.contains("running=false"),
            "got: {stopped}"
        );
        svc.shutdown().await;
    }

    #[tokio::test]
    async fn poll_unknown_job_is_clean_error() {
        let (_ctx, tools, _svc) = ctx_with_shell();
        let err = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_poll", serde_json::json!({"job_id": "sh-4242"})),
            )
            .await
            .unwrap_err();
        assert_eq!(err.tool, "shell_poll");
    }

    #[tokio::test]
    async fn explicit_env_gives_clean_subprocess() {
        let (_ctx, tools, _svc) = ctx_with_shell();
        let py = [
            "/etc/profiles/per-user/pranesh/bin/python3",
            "/usr/bin/python3",
            "/usr/local/bin/python3",
        ]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or(&"python3")
        .to_string();
        // python3 prints ONLY_THIS; a leaked PATH would show Some(...).
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": [py, "-c", "import os; print(os.environ.get('ONLY_THIS'), os.environ.get('PATH'))"],
                        "env": {"ONLY_THIS": "yes"}}),
                ),
            )
            .await
            .unwrap();
        assert!(out.contains("yes None"), "got: {out}");
    }

    #[test]
    fn specs_are_advertisable() {
        for spec in [
            shell_exec_spec(),
            shell_start_spec(),
            shell_poll_spec(),
            shell_stop_spec(),
        ] {
            assert!(!spec.name.is_empty());
            assert!(spec.parameters.get("type").is_some());
        }
    }
}
