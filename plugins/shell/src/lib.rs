//! Headless shell tool plugin: safe, bounded, non-interactive subprocesses.
//!
//! Four tools over one [`ShellService`]:
//! `shell_exec` (sync), `shell_start` / `shell_poll` / `shell_stop`
//! (background). Every call passes through [`CompiledPolicy`] first; output
//! is tail-truncated and labeled; background jobs can never stall the loop
//! (`start` returns an id immediately, `poll` is a non-blocking snapshot).
//!
//! Safety is best-effort, not a sandbox: an allowlisted binary can still be
//! abused (`python3 -c ...`). Run untrusted work in a container.

mod env;
mod exec;
mod jobs;
mod output;
mod policy;

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use harness_config::{AppConfig, ShellConfig, ShellEnvMode};
use harness_contracts::{KEY_TOOLS, ToolError, ToolSpec};
use harness_core::{Context, Result};

pub use harness_config::LLM_API_KEY_ENV_PATTERNS;
pub use jobs::JobLimits;
pub use output::{Tail, format_result, format_result_full, tail_truncate};
pub use policy::CompiledPolicy;

use exec::resolve_request;
use jobs::{JobManager, validate_env_keys};

const SAFETY_NOTE: &str = "Best-effort safety net, not a sandbox: argv-only (no shell), strict allowlist, dangerous operators and TTY commands blocked. Run untrusted work in a container.";

fn tool_err(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError {
        tool: tool.to_owned(),
        message: message.into(),
    }
}

/// Runtime service behind the four shell tools. Clone via `Arc`.
pub struct ShellService {
    cfg: ShellConfig,
    policy: CompiledPolicy,
    jobs: JobManager,
}

impl ShellService {
    pub fn new(cfg: ShellConfig) -> Self {
        let policy = CompiledPolicy::compile(&cfg);
        let limits = JobLimits::from_config(&cfg);
        ShellService {
            cfg,
            policy,
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

    fn denied_env(&self) -> &[String] {
        &self.cfg.env.denied_patterns
    }

    fn env_mode(&self) -> ShellEnvMode {
        self.cfg.env.mode
    }

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
        self.policy.check(argv).map_err(|e| tool_err(tool, e))?;
        // Spawn the original argv[0] (not the policy basename) so absolute
        // paths keep working, e.g. under a clean env with no PATH.
        Ok(argv[0].trim().to_owned())
    }

    /// Synchronous execution. Non-zero exits are `Ok` (labeled output);
    /// policy/spawn failures are `Err`.
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
        exec::run_once(&self.cfg, req, self.denied_env(), self.env_mode())
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
            .start(
                &self.cfg,
                exe,
                argv,
                workdir,
                timeout_ms,
                env,
                self.denied_env(),
                self.env_mode(),
            )
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
// Handlers (each: parse -> service call; service enforces policy)
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
            "command": { "type": "array", "items": { "type": "string" }, "description": "argv array, e.g. [\"git\", \"status\"]. No shell; operators like > | ; are rejected." },
            "workdir": { "type": "string", "description": "Working directory (must be under allowed_workdirs, default cwd)" },
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
            "Run a command synchronously and return labeled tail-truncated [stdout]/[stderr]. {SAFETY_NOTE}"
        ),
        parameters: exec_params(),
    }
}

pub fn shell_start_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_start".to_owned(),
        description: format!(
            "Start a long-running command in the background; returns a job id immediately (never blocks). Poll with shell_poll, end with shell_stop. {SAFETY_NOTE}"
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

/// Headless shell plugin. Build from `[shell]` config or defaults.
///
/// ```rust,no_run
/// # use harness_shell::ShellPlugin;
/// # use harness_config::AppConfig;
/// let plugin = ShellPlugin::default();
/// ```
#[derive(Default)]
pub struct ShellPlugin {
    config: ShellConfig,
}

impl ShellPlugin {
    pub fn new(config: ShellConfig) -> Self {
        ShellPlugin { config }
    }

    /// Build from the full app config (`[shell]` section).
    pub fn from_app_config(cfg: &AppConfig) -> Self {
        ShellPlugin {
            config: cfg.shell.clone(),
        }
    }
}

impl harness_core::Plugin for ShellPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("shell").injects(KEY_TOOLS)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        use harness_tools::Tools;
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS)?;
        let svc = Arc::new(ShellService::new(self.config.clone()));
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
        // background jobs via JobManager::drop. Stored under an internal key
        // owned by this plugin so unload cleans it up.
        ctx.provide_key("shell.service", svc);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_TOOLS, ToolCall};
    use harness_tools::{Tools, ToolsPlugin};

    fn test_config() -> ShellConfig {
        let mut cfg = ShellConfig::default();
        for extra in ["python3", "sleep", "sh"] {
            if !cfg.allowlist.iter().any(|s| s == extra) {
                cfg.allowlist.push(extra.to_owned());
            }
        }
        cfg
    }

    fn ctx_with_shell(cfg: ShellConfig) -> (Context, Arc<Tools>, Arc<ShellService>) {
        let ctx = Context::root();
        ctx.load(ToolsPlugin).unwrap();
        ctx.load(ShellPlugin::new(cfg)).unwrap();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let svc: Arc<ShellService> = ctx.inject_key("shell.service").unwrap();
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
    async fn exec_tool_runs_allowlisted_command() {
        let (_ctx, tools, _svc) = ctx_with_shell(test_config());
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
    async fn exec_tool_denies_rm() {
        let (_ctx, tools, _svc) = ctx_with_shell(test_config());
        let err = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": ["rm", "-rf", "/"]}),
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(err.tool, "shell_exec");
        assert!(err.message.contains("denylist") || err.message.contains("allowlist"));
    }

    #[tokio::test]
    async fn exec_tool_denies_unknown_binary() {
        let (_ctx, tools, _svc) = ctx_with_shell(test_config());
        let err = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": ["evilminer", "--go"]}),
                ),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("allowlist"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn exec_tool_denies_operators_and_shell_strings() {
        let (_ctx, tools, _svc) = ctx_with_shell(test_config());
        let err = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": ["echo", "a", ">", "/tmp/x"]}),
                ),
            )
            .await
            .unwrap_err();
        assert!(err.message.contains("operator"), "got: {}", err.message);
        // Single-string shell command: `sh` not allowlisted by default config,
        // but test config adds it with allow_shell=false -> blocked.
        let err2 = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": ["sh", "-c", "echo hi"]}),
                ),
            )
            .await
            .unwrap_err();
        assert!(
            err2.message.contains("shell") || err2.message.contains("allow"),
            "got: {}",
            err2.message
        );
    }

    #[tokio::test]
    async fn background_lifecycle_through_registry() {
        let (_ctx, tools, svc) = ctx_with_shell(test_config());
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
        let (_ctx, tools, _svc) = ctx_with_shell(test_config());
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
        let (_ctx, tools, _svc) = ctx_with_shell(test_config());
        // Clean env has no PATH, so resolve the interpreter absolutely
        // (basename must stay `python3` for the allowlist; do NOT
        // canonicalize — it may resolve a versioned symlink target).
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
