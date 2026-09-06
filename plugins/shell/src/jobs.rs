//! Background jobs: non-blocking start/poll/stop over a bounded map.
//!
//! `start` returns immediately (never stalls the agent loop); `poll` is a
//! lock-and-copy snapshot, O(tail). Every job is bounded by `max_job_time`,
//! `ring_cap` per stream, and `max_jobs` map-wide. Cleanup is layered:
//! `kill_on_drop` on every child + waiter-owned `Child` + `Drop`/`shutdown`
//! aborting all tasks (abort drops the waiter, which drops/kills the child).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify};

use crate::env::apply_env;
use crate::exec::{Ring, drain_into, resolve_workdir};
use crate::output::tail_truncate;
use harness_config::{ShellConfig, ShellEnvMode};

/// Limits snapshot copied out of `ShellConfig` at build.
#[derive(Debug, Clone, Copy)]
pub struct JobLimits {
    pub max_jobs: usize,
    pub max_job_time: Duration,
    pub ring_cap: usize,
    pub tail_cap: usize,
}

impl JobLimits {
    pub fn from_config(cfg: &ShellConfig) -> Self {
        JobLimits {
            max_jobs: cfg.max_jobs.max(1),
            max_job_time: Duration::from_millis(cfg.max_job_time_ms.max(1)),
            ring_cap: cfg.max_job_output_bytes.max(1024),
            tail_cap: cfg.max_output_bytes.max(1024),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct JobExit {
    code: Option<i32>,
    expired: bool,
    stopped: bool,
}

struct JobHandle {
    cmdline: String,
    started: Instant,
    out: Arc<Mutex<Ring>>,
    err: Arc<Mutex<Ring>>,
    exit: Arc<Mutex<Option<JobExit>>>,
    kill: Arc<Notify>,
    tasks: Vec<tokio::task::AbortHandle>,
}

pub struct JobManager {
    inner: std::sync::Mutex<HashMap<String, JobHandle>>,
    next: AtomicU64,
    limits: JobLimits,
}

impl JobManager {
    pub fn new(limits: JobLimits) -> Self {
        JobManager {
            inner: std::sync::Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            limits,
        }
    }

    pub fn job_count(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Spawn a background job; returns the job id immediately.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        cfg: &ShellConfig,
        exe: String,
        argv: Vec<String>,
        workdir: Option<String>,
        deadline_override_ms: Option<u64>,
        explicit_env: Option<std::collections::HashMap<String, String>>,
        denied_env: &[String],
        env_mode: ShellEnvMode,
    ) -> Result<String, String> {
        // Reuse workdir validation; argv already policy-checked by caller.
        let workdir: PathBuf = resolve_workdir(&cfg.allowed_workdirs, workdir.as_deref())?;
        let deadline = match deadline_override_ms {
            Some(ms) => Duration::from_millis(ms.clamp(1, cfg.max_job_time_ms.max(1))),
            None => self.limits.max_job_time,
        };
        // Validate env keys early ( MISUSE gives clean errors, no spawn).
        if let Some(vars) = explicit_env.as_ref() {
            for k in vars.keys() {
                if k.is_empty() || k.contains('=') || k.contains('\0') {
                    return Err(format!("invalid env key `{k}`"));
                }
            }
        }

        {
            let mut map = self
                .inner
                .lock()
                .map_err(|_| "job table poisoned".to_owned())?;
            // Evict finished jobs first (best-effort: skip try_lock failures).
            if map.len() >= self.limits.max_jobs {
                let finished: Vec<String> = map
                    .iter()
                    .filter(|(_, h)| h.exit.try_lock().map(|g| g.is_some()).unwrap_or(false))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in finished {
                    map.remove(&id);
                }
            }
            if map.len() >= self.limits.max_jobs {
                return Err(format!(
                    "too many background jobs ({}/{}); stop one with shell_stop",
                    map.len(),
                    self.limits.max_jobs
                ));
            }
            let _ = &mut map;
        }

        let mut cmd = tokio::process::Command::new(&exe);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        cmd.current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        apply_env(
            cmd.as_std_mut(),
            explicit_env.as_ref(),
            env_mode,
            denied_env,
        );
        let mut child = cmd.spawn().map_err(|e| format!("spawn `{exe}`: {e}"))?;

        let id = format!("sh-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let out = Arc::new(Mutex::new(Ring::default()));
        let err = Arc::new(Mutex::new(Ring::default()));
        let exit = Arc::new(Mutex::new(None::<JobExit>));
        let kill = Arc::new(Notify::new());
        let mut tasks = Vec::with_capacity(3);
        let ring_cap = self.limits.ring_cap;

        if let Some(p) = child.stdout.take() {
            tasks.push(tokio::spawn(drain_into(p, Arc::clone(&out), ring_cap)).abort_handle());
        }
        if let Some(p) = child.stderr.take() {
            tasks.push(tokio::spawn(drain_into(p, Arc::clone(&err), ring_cap)).abort_handle());
        }
        {
            let exit_w = Arc::clone(&exit);
            let kill_w = Arc::clone(&kill);
            let waiter = tokio::spawn(async move {
                waiter_task(child, exit_w, kill_w, deadline).await;
            });
            tasks.push(waiter.abort_handle());
        }

        let cmdline = argv.join(" ");
        let mut map = self
            .inner
            .lock()
            .map_err(|_| "job table poisoned".to_owned())?;
        map.insert(
            id.clone(),
            JobHandle {
                cmdline,
                started: Instant::now(),
                out,
                err,
                exit,
                kill,
                tasks,
            },
        );
        Ok(id)
    }

    /// Non-blocking snapshot of a job. Never awaits the child.
    pub async fn poll(&self, id: &str, tail_bytes: Option<usize>) -> Result<String, String> {
        let (handle_refs, tail_cap) = {
            let map = self
                .inner
                .lock()
                .map_err(|_| "job table poisoned".to_owned())?;
            let h = map.get(id).ok_or_else(|| format!("unknown job `{id}`"))?;
            (
                (
                    h.cmdline.clone(),
                    h.started,
                    Arc::clone(&h.out),
                    Arc::clone(&h.err),
                    Arc::clone(&h.exit),
                ),
                tail_bytes
                    .unwrap_or(self.limits.tail_cap)
                    .clamp(1, self.limits.ring_cap),
            )
        };
        let (cmdline, started, out, err, exit) = handle_refs;
        let ((o, o_omit), (e, e_omit), x) = tokio::join!(
            async { out.lock().await.snapshot_with_omitted() },
            async { err.lock().await.snapshot_with_omitted() },
            async { *exit.lock().await }
        );
        Ok(format_job(
            id,
            &cmdline,
            started.elapsed(),
            x,
            &o,
            &e,
            tail_cap,
            o_omit,
            e_omit,
        ))
    }

    /// Signal a job to stop; waits up to ~2s for the waiter to reap it.
    pub async fn stop(&self, id: &str) -> Result<String, String> {
        let (kill, exit_ref) = {
            let map = self
                .inner
                .lock()
                .map_err(|_| "job table poisoned".to_owned())?;
            let h = map.get(id).ok_or_else(|| format!("unknown job `{id}`"))?;
            (Arc::clone(&h.kill), Arc::clone(&h.exit))
        };
        if exit_ref.lock().await.is_some() {
            let snap = self.poll(id, None).await?;
            return Ok(format!("already finished\n{snap}"));
        }
        kill.notify_one();
        // Bounded wait for the waiter to kill+reap (never stalls the loop).
        for _ in 0..20 {
            if exit_ref.lock().await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Ensure the stopped flag is visible if waiter hasn't recorded yet.
        {
            let mut g = exit_ref.lock().await;
            if let Some(x) = g.as_mut() {
                x.stopped = true;
            } else {
                *g = Some(JobExit {
                    code: None,
                    expired: false,
                    stopped: true,
                });
            }
        }
        let snap = self.poll(id, None).await?;
        Ok(format!("stopped\n{snap}"))
    }

    /// Kill all jobs and clear the table (harness exit / tests).
    pub async fn shutdown(&self) {
        let handles: Vec<(Arc<Notify>, Vec<tokio::task::AbortHandle>)> = {
            match self.inner.lock() {
                Ok(map) => map
                    .values()
                    .map(|h| (Arc::clone(&h.kill), h.tasks.clone()))
                    .collect(),
                Err(_) => Vec::new(),
            }
        };
        for (kill, _) in &handles {
            kill.notify_waiters();
        }
        // Grace period for waiters to reap (bounded, non-blocking to loop).
        tokio::time::sleep(Duration::from_millis(500)).await;
        for (_, tasks) in &handles {
            for t in tasks {
                t.abort();
            }
        }
        if let Ok(mut map) = self.inner.lock() {
            map.clear();
        }
    }

    /// Abort every job task. Sync so `Drop` can call it; aborting the waiter
    /// drops its owned `Child`, and `kill_on_drop(true)` kills the process.
    fn abort_all_sync(&self) {
        if let Ok(map) = self.inner.lock() {
            for h in map.values() {
                h.kill.notify_waiters();
                for t in &h.tasks {
                    t.abort();
                }
            }
        }
    }
}

impl Drop for JobManager {
    fn drop(&mut self) {
        self.abort_all_sync();
    }
}

async fn waiter_task(
    mut child: tokio::process::Child,
    exit: Arc<Mutex<Option<JobExit>>>,
    kill: Arc<Notify>,
    deadline: Duration,
) {
    enum Outcome {
        Exited,
        Killed,
        Expired,
    }
    let outcome = tokio::select! {
        s = child.wait() => {
            let code = s.map(|st| st.code()).unwrap_or(None);
            *exit.lock().await = Some(JobExit { code, expired: false, stopped: false });
            Outcome::Exited
        }
        _ = kill.notified() => Outcome::Killed,
        _ = tokio::time::sleep(deadline) => Outcome::Expired,
    };
    match outcome {
        Outcome::Exited => {}
        Outcome::Killed => {
            let _ = child.start_kill();
            let code = tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .ok()
                .and_then(|r| r.ok())
                .and_then(|st| st.code());
            let mut g = exit.lock().await;
            // stop() may have written a placeholder; fill in the real code.
            *g = Some(JobExit {
                code,
                expired: false,
                stopped: true,
            });
        }
        Outcome::Expired => {
            let _ = child.start_kill();
            let code = tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .ok()
                .and_then(|r| r.ok())
                .and_then(|st| st.code());
            *exit.lock().await = Some(JobExit {
                code,
                expired: true,
                stopped: false,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn format_job(
    id: &str,
    cmdline: &str,
    age: Duration,
    exit: Option<JobExit>,
    stdout: &[u8],
    stderr: &[u8],
    tail_cap: usize,
    ring_omitted_out: u64,
    ring_omitted_err: u64,
) -> String {
    let out = tail_truncate(stdout, tail_cap);
    let err = tail_truncate(stderr, tail_cap);
    let omitted_out = out.omitted_bytes + ring_omitted_out;
    let omitted_err = err.omitted_bytes + ring_omitted_err;
    let mut s = String::with_capacity(out.text.len() + err.text.len() + 160);
    match exit {
        None => s.push_str(&format!(
            "[job {id}, running=true, age_ms={}]\ncmd: {cmdline}\n",
            age.as_millis()
        )),
        Some(x) => s.push_str(&format!(
            "[job {id}, running=false, exit={}, expired={}, stopped={}, age_ms={}]\ncmd: {cmdline}\n",
            x.code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_owned()),
            x.expired,
            x.stopped,
            age.as_millis()
        )),
    }
    if out.truncated || err.truncated || omitted_out > 0 || omitted_err > 0 {
        s.push_str(&format!(
            "[truncated=true omitted_stdout={}B omitted_stderr={}B]\n",
            omitted_out, omitted_err
        ));
    }
    s.push_str("[stdout]\n");
    s.push_str(&out.text);
    if !out.text.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("[stderr]\n");
    s.push_str(&err.text);
    if !err.text.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Validate per-call env keys without spawning.
pub fn validate_env_keys(
    vars: Option<&std::collections::HashMap<String, String>>,
) -> Result<(), String> {
    if let Some(vars) = vars {
        for k in vars.keys() {
            if k.is_empty() || k.contains('=') || k.contains('\0') {
                return Err(format!("invalid env key `{k}`"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_config::ShellEnvMode;

    fn test_config() -> ShellConfig {
        let mut cfg = ShellConfig::default();
        cfg.allowlist.push("python3".to_owned());
        cfg.allowlist.push("echo".to_owned());
        cfg
    }

    #[tokio::test]
    async fn start_poll_stop_lifecycle() {
        let cfg = test_config();
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        let id = mgr
            .start(
                &cfg,
                "python3".to_owned(),
                vec![
                    "python3".to_owned(),
                    "-c".to_owned(),
                    "import time; time.sleep(30)".to_owned(),
                ],
                None,
                None,
                None,
                &[],
                ShellEnvMode::InheritFiltered,
            )
            .await
            .unwrap();
        let snap = mgr.poll(&id, None).await.unwrap();
        assert!(snap.contains("running=true"), "got: {snap}");
        let stopped = mgr.stop(&id).await.unwrap();
        assert!(
            stopped.contains("stopped") || stopped.contains("running=false"),
            "{stopped}"
        );
        let snap2 = mgr.poll(&id, None).await.unwrap();
        assert!(snap2.contains("running=false"), "got: {snap2}");
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn finished_job_reports_exit() {
        let cfg = test_config();
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        let id = mgr
            .start(
                &cfg,
                "echo".to_owned(),
                vec!["echo".to_owned(), "bg-hello".to_owned()],
                None,
                None,
                None,
                &[],
                ShellEnvMode::InheritFiltered,
            )
            .await
            .unwrap();
        // Wait bounded for natural exit.
        let mut snap = String::new();
        for _ in 0..50 {
            snap = mgr.poll(&id, None).await.unwrap();
            if snap.contains("running=false") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(snap.contains("running=false"), "got: {snap}");
        assert!(snap.contains("bg-hello"), "got: {snap}");
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_job_errors() {
        let cfg = test_config();
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        assert!(mgr.poll("sh-999", None).await.is_err());
        assert!(mgr.stop("sh-999").await.is_err());
    }

    #[tokio::test]
    async fn max_jobs_enforced() {
        let mut cfg = test_config();
        cfg.max_jobs = 1;
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        let mk = || {
            mgr.start(
                &cfg,
                "python3".to_owned(),
                vec![
                    "python3".to_owned(),
                    "-c".to_owned(),
                    "import time; time.sleep(30)".to_owned(),
                ],
                None,
                None,
                None,
                &[],
                ShellEnvMode::InheritFiltered,
            )
        };
        mk().await.unwrap();
        assert!(mk().await.is_err());
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn expiry_kills_long_job() {
        let mut cfg = test_config();
        cfg.max_job_time_ms = 400;
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        let id = mgr
            .start(
                &cfg,
                "python3".to_owned(),
                vec![
                    "python3".to_owned(),
                    "-c".to_owned(),
                    "import time; time.sleep(30)".to_owned(),
                ],
                None,
                None,
                None,
                &[],
                ShellEnvMode::InheritFiltered,
            )
            .await
            .unwrap();
        let mut snap = String::new();
        for _ in 0..50 {
            snap = mgr.poll(&id, None).await.unwrap();
            if snap.contains("running=false") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(snap.contains("expired=true"), "got: {snap}");
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_clears_table() {
        let cfg = test_config();
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        mgr.start(
            &cfg,
            "python3".to_owned(),
            vec![
                "python3".to_owned(),
                "-c".to_owned(),
                "import time; time.sleep(30)".to_owned(),
            ],
            None,
            None,
            None,
            &[],
            ShellEnvMode::InheritFiltered,
        )
        .await
        .unwrap();
        assert_eq!(mgr.job_count(), 1);
        mgr.shutdown().await;
        assert_eq!(mgr.job_count(), 0);
    }
}
