//! Headless shell tool plugin over a persistent bash session.
//!
//! Four tools over one [`ShellService`]:
//! `shell_exec` (sync, in-session), `shell_start` / `shell_poll` /
//! `shell_stop` (background, one-shot `bash -c`). No policy guardrails live
//! here: any command runs with the agent's environment. `shell_exec` takes
//! a shell command *string* run in a long-lived bash: pipes, redirects,
//! `&&`, `;` all work, and `cd`/env changes persist across calls. Output
//! is quiet on success (raw stdout) with a one-line trailer otherwise;
//! background jobs can never stall the loop (`start` returns an id
//! immediately, `poll` long-polls up to a bounded `wait_ms` and returns
//! early on exit).
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
mod session;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use harness_config::{AppConfig, ShellConfig};
use harness_contracts::{KEY_CONFIG, KEY_SHELL_SERVICE, KEY_TOOLS, ToolError, ToolSpec};
use harness_core::{Context, Result};

pub use jobs::{DEFAULT_POLL_WAIT_MS, JobLimits, MAX_POLL_WAIT_MS};
pub use output::{Tail, format_result, format_result_full, format_shell_result, tail_truncate};

use exec::{resolve_request, resolve_workdir};
use jobs::{JobManager, validate_env_keys};
use session::BashSession;

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
    session: BashSession,
}

impl ShellService {
    pub fn new(cfg: ShellConfig) -> Self {
        let limits = JobLimits::from_config(&cfg);
        ShellService {
            cfg,
            jobs: JobManager::new(limits),
            session: BashSession::new(),
        }
    }

    pub fn config(&self) -> &ShellConfig {
        &self.cfg
    }

    pub fn job_count(&self) -> usize {
        self.jobs.job_count()
    }

    /// Graceful shutdown: kill the session shell and all background jobs
    /// (harness exit / tests).
    pub async fn shutdown(&self) {
        self.session.shutdown().await;
        self.jobs.shutdown().await;
    }

    /// Argument-shape validation only (no policy).
    fn check_common(
        &self,
        tool: &str,
        command: &str,
        env: &Option<HashMap<String, String>>,
    ) -> std::result::Result<(), ToolError> {
        if command.trim().is_empty() {
            return Err(tool_err(tool, "command must be a non-empty string"));
        }
        if command.len() > 64 * 1024 {
            return Err(tool_err(tool, "command is too long (max 64KiB total)"));
        }
        validate_env_keys(env.as_ref()).map_err(|e| tool_err(tool, e))?;
        Ok(())
    }

    /// Synchronous execution in the persistent bash session. Clean runs
    /// return raw stdout; failures carry streams plus a one-line trailer.
    /// An explicit `env` bypasses the session into an isolated one-shot
    /// `bash -c` with ONLY these vars (the session environment is never
    /// polluted). Only spawn-level failures are `Err`.
    pub async fn exec_sync(
        &self,
        tool: &'static str,
        command: String,
        workdir: Option<String>,
        timeout_ms: Option<u64>,
        env: Option<HashMap<String, String>>,
    ) -> std::result::Result<String, ToolError> {
        self.check_common(tool, &command, &env)?;
        let want = timeout_ms.unwrap_or(self.cfg.default_timeout_ms);
        let timeout = Duration::from_millis(want.clamp(1, self.cfg.max_timeout_ms.max(1)));
        if env.is_some() {
            let req = resolve_request(
                &self.cfg,
                "bash".to_owned(),
                vec!["bash".to_owned(), "-c".to_owned(), command],
                workdir,
                Some(timeout.as_millis() as u64),
                env,
            )
            .map_err(|e| tool_err(tool, e))?;
            return exec::run_once(&self.cfg, req)
                .await
                .map_err(|e| tool_err(tool, e));
        }
        let dir = match workdir {
            None => None,
            Some(w) => Some(resolve_workdir(Some(w.as_str())).map_err(|e| tool_err(tool, e))?),
        };
        let out = self
            .session
            .run(
                &command,
                dir.as_deref(),
                timeout,
                self.cfg.max_capture_bytes.max(1024),
            )
            .await
            .map_err(|e| tool_err(tool, e))?;
        Ok(format_shell_result(
            out.exit_code,
            out.timed_out,
            &out.stdout,
            &out.stderr,
            self.cfg.max_output_bytes,
            out.out_omitted,
            out.err_omitted,
        ))
    }

    /// Launch a background job (`bash -c` one-shot); returns immediately
    /// with the job id.
    pub async fn start(
        &self,
        tool: &'static str,
        command: String,
        workdir: Option<String>,
        timeout_ms: Option<u64>,
        env: Option<HashMap<String, String>>,
    ) -> std::result::Result<String, ToolError> {
        self.check_common(tool, &command, &env)?;
        let id = self
            .jobs
            .start(
                &self.cfg,
                "bash".to_owned(),
                vec!["bash".to_owned(), "-c".to_owned(), command],
                workdir,
                timeout_ms,
                env,
            )
            .await
            .map_err(|e| tool_err(tool, e))?;
        Ok(format!(
            "started {id}\nuse shell_poll with job_id \"{id}\" to read output"
        ))
    }

    /// Output snapshot for a job, optionally long-polling for completion.
    /// `wait_ms` omits to ~2s; `Some(0)` is instant. Bounded, early-exits.
    pub async fn poll(
        &self,
        tool: &'static str,
        job_id: &str,
        tail_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> std::result::Result<String, ToolError> {
        if job_id.trim().is_empty() {
            return Err(tool_err(tool, "job_id must not be empty"));
        }
        self.jobs
            .poll(job_id, tail_bytes, wait_ms)
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
    command: String,
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
    #[serde(default)]
    wait_ms: Option<u64>,
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
        svc.poll("shell_poll", &a.job_id, a.tail_bytes, a.wait_ms)
            .await
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
            "command": { "type": "string", "description": "Shell command string, e.g. \"ls -la; cat package.json | head -n 100\". Runs in a persistent bash session: pipes, redirects, &&, ; all work, and cwd/env changes persist across calls." },
            "workdir": { "type": "string", "description": "Working directory for this call only (must exist, default session cwd)" },
            "timeout_ms": { "type": "integer", "minimum": 1, "description": "Timeout override, clamped to max_timeout_ms" },
            "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "If given, the command runs one-shot with ONLY these vars (clean env, outside the session)" }
        },
        "required": ["command"]
    })
}

pub fn shell_exec_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_exec".to_owned(),
        description: format!(
            "Run a shell command string in the persistent bash session and return its output. {NO_POLICY_NOTE}"
        ),
        parameters: exec_params(),
    }
}

pub fn shell_start_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_start".to_owned(),
        description: format!(
            "Start a shell command string in the background (bash -c); returns a job id immediately (never blocks). Poll with shell_poll, end with shell_stop. {NO_POLICY_NOTE}"
        ),
        parameters: exec_params(),
    }
}

pub fn shell_poll_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_poll".to_owned(),
        description: "Snapshot of a background job: waits up to wait_ms for exit (early-exits), then running state plus output streams when non-empty. Prefer one poll with a generous wait_ms over tight polling.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "Job id from shell_start (e.g. sh-1)" },
                "tail_bytes": { "type": "integer", "minimum": 1, "description": "Tail bytes per stream" },
                "wait_ms": { "type": "integer", "minimum": 0, "maximum": 30000, "description": "Long-poll up to N ms for exit, returning early when the job finishes. Omit for ~2000ms. Use 0 for an instant snapshot." }
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
        ctx.load(harness_config::ConfigPlugin::from_toml(TEST_CONFIG_TOML).unwrap())
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
    async fn exec_tool_runs_shell_strings() {
        let (_ctx, tools, _svc) = ctx_with_shell();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_exec", serde_json::json!({"command": "echo hi"})),
            )
            .await
            .unwrap();
        assert_eq!(out, "hi\n", "quiet success returns raw stdout: {out:?}");
    }

    #[tokio::test]
    async fn exec_tool_runs_pipelines_and_chains() {
        // The exact shape that failed as argv now works as a shell string.
        let (_ctx, tools, _svc) = ctx_with_shell();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": "pwd; ls -la | head -n 3; echo hi | tr a-z A-Z"}),
                ),
            )
            .await
            .unwrap();
        assert!(out.contains("HI"), "got: {out}");
        assert!(out.contains("total"), "got: {out}");
    }

    #[tokio::test]
    async fn exec_tool_rejects_bad_commands() {
        let (_ctx, tools, _svc) = ctx_with_shell();
        // Empty string.
        let err = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_exec", serde_json::json!({"command": ""})),
            )
            .await
            .unwrap_err();
        assert_eq!(err.tool, "shell_exec");
        // Missing key.
        let err = tools
            .execute("a", "s", 1, call("shell_exec", serde_json::json!({})))
            .await
            .unwrap_err();
        assert_eq!(err.tool, "shell_exec");
        // Old argv shape is a type error, not a silent misroute.
        let err = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_exec", serde_json::json!({"command": ["echo", "hi"]})),
            )
            .await
            .unwrap_err();
        assert_eq!(err.tool, "shell_exec");
    }

    #[tokio::test]
    async fn exec_tool_reports_failures_with_trailer() {
        let (_ctx, tools, _svc) = ctx_with_shell();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": "echo out; echo err >&2; exit 3"}),
                ),
            )
            .await
            .unwrap();
        assert!(out.contains("[exit 3"), "got: {out}");
        assert!(out.contains("[stderr]"), "got: {out}");
        assert!(out.contains("out") && out.contains("err"), "got: {out}");
    }

    #[tokio::test]
    async fn exec_tool_session_state_persists() {
        let (_ctx, tools, _svc) = ctx_with_shell();
        let unique = format!("HARNESS_PERSIST_{}", std::process::id());
        tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": format!("export {unique}=yes")}),
                ),
            )
            .await
            .unwrap();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": format!("echo ${unique}")}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(out, "yes\n", "export persisted: {out:?}");

        tools
            .execute(
                "a",
                "s",
                1,
                call("shell_exec", serde_json::json!({"command": "cd /"})),
            )
            .await
            .unwrap();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_exec", serde_json::json!({"command": "pwd"})),
            )
            .await
            .unwrap();
        assert_eq!(out.trim(), "/");

        // Explicit workdir scopes one call; the session cwd is untouched.
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": "pwd", "workdir": "/tmp"}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(out.trim(), "/tmp");
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_exec", serde_json::json!({"command": "pwd"})),
            )
            .await
            .unwrap();
        assert_eq!(out.trim(), "/");
    }

    #[tokio::test]
    async fn background_lifecycle_through_registry() {
        let (_ctx, tools, svc) = ctx_with_shell();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_start", serde_json::json!({"command": "sleep 30"})),
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
                call(
                    "shell_poll",
                    serde_json::json!({"job_id": id, "wait_ms": 0}),
                ),
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
        // One-shot bypasses the session with ONLY_THIS set; a leaked PATH
        // would show Some(...). Single-quoted shell keeps it intact.
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_exec",
                    serde_json::json!({"command": format!("{py} -c 'import os; print(os.environ.get(\"ONLY_THIS\"), os.environ.get(\"PATH\"))'"),
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
        let poll_params = shell_poll_spec().parameters;
        assert!(
            poll_params["properties"].get("wait_ms").is_some(),
            "poll must advertise wait_ms: {poll_params}"
        );
    }

    #[tokio::test]
    async fn poll_long_waits_resolve_quick_job_in_one_call() {
        let (_ctx, tools, svc) = ctx_with_shell();
        let out = tools
            .execute(
                "a",
                "s",
                1,
                call("shell_start", serde_json::json!({"command": "echo waited"})),
            )
            .await
            .unwrap();
        let id: String = out
            .split_whitespace()
            .find(|w| w.starts_with("sh-"))
            .unwrap()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_owned();
        let snap = tools
            .execute(
                "a",
                "s",
                1,
                call(
                    "shell_poll",
                    serde_json::json!({"job_id": id, "wait_ms": 5000}),
                ),
            )
            .await
            .unwrap();
        assert!(snap.contains("running=false"), "got: {snap}");
        assert!(snap.contains("waited"), "got: {snap}");
        svc.shutdown().await;
    }
}
