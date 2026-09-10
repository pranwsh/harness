//! Background jobs: non-blocking start, long-polling poll, bounded stop.
//!
//! `start` returns immediately (never stalls the agent loop); `poll` waits up
//! to a bounded `wait_ms` for exit (early-exits) then takes a lock-and-copy
//! snapshot, O(tail). Every job is bounded by `max_job_time`,
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
use harness_config::ShellConfig;

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

/// Default long-poll wait for `poll` when the caller omits `wait_ms`.
pub const DEFAULT_POLL_WAIT_MS: u64 = 2_000;
/// Hard clamp for `poll`'s `wait_ms` so one poll can never stall the loop.
pub const MAX_POLL_WAIT_MS: u64 = 30_000;

struct JobHandle {
    started: Instant,
    out: Arc<Mutex<Ring>>,
    err: Arc<Mutex<Ring>>,
    exit: Arc<Mutex<Option<JobExit>>>,
    kill: Arc<Notify>,
    done: Arc<Notify>,
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
    /// No policy checks: any executable runs. Resource bounds
    /// (`max_jobs`, `max_job_time`) still apply.
    pub async fn start(
        &self,
        cfg: &ShellConfig,
        exe: String,
        argv: Vec<String>,
        workdir: Option<String>,
        deadline_override_ms: Option<u64>,
        explicit_env: Option<std::collections::HashMap<String, String>>,
    ) -> Result<String, String> {
        let workdir: PathBuf = resolve_workdir(workdir.as_deref())?;
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
        apply_env(cmd.as_std_mut(), explicit_env.as_ref());
        let mut child = cmd.spawn().map_err(|e| format!("spawn `{exe}`: {e}"))?;

        let id = format!("sh-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let out = Arc::new(Mutex::new(Ring::default()));
        let err = Arc::new(Mutex::new(Ring::default()));
        let exit = Arc::new(Mutex::new(None::<JobExit>));
        let kill = Arc::new(Notify::new());
        let done = Arc::new(Notify::new());
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
            let done_w = Arc::clone(&done);
            let waiter = tokio::spawn(async move {
                waiter_task(child, exit_w, kill_w, done_w, deadline).await;
            });
            tasks.push(waiter.abort_handle());
        }

        let mut map = self
            .inner
            .lock()
            .map_err(|_| "job table poisoned".to_owned())?;
        map.insert(
            id.clone(),
            JobHandle {
                started: Instant::now(),
                out,
                err,
                exit,
                kill,
                done,
                tasks,
            },
        );
        Ok(id)
    }

    /// Snapshot of a job, optionally long-polling for completion.
    ///
    /// When `wait_ms` is `None` the call waits `DEFAULT_POLL_WAIT_MS`;
    /// `Some(0)` is an instant snapshot. The wait is bounded by
    /// `MAX_POLL_WAIT_MS` and returns early as soon as the job exits, so one
    /// poll can cover wall-clock time without ever stalling the agent loop.
    pub async fn poll(
        &self,
        id: &str,
        tail_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> Result<String, String> {
        let (handle_refs, tail_cap, wait) = {
            let map = self
                .inner
                .lock()
                .map_err(|_| "job table poisoned".to_owned())?;
            let h = map.get(id).ok_or_else(|| format!("unknown job `{id}`"))?;
            (
                (
                    h.started,
                    Arc::clone(&h.out),
                    Arc::clone(&h.err),
                    Arc::clone(&h.exit),
                    Arc::clone(&h.done),
                ),
                tail_bytes
                    .unwrap_or(self.limits.tail_cap)
                    .clamp(1, self.limits.ring_cap),
                wait_ms
                    .unwrap_or(DEFAULT_POLL_WAIT_MS)
                    .clamp(0, MAX_POLL_WAIT_MS),
            )
        };
        let (started, out, err, exit, done) = handle_refs;
        // Subscribe before reading `exit`: the waiter writes `exit` before
        // notifying `done`, so either we see `Some` and skip the wait, or we
        // are already subscribed and cannot miss the wakeup. A missed `stop`
        // placeholder still resolves via the bounded timeout below.
        if wait > 0 {
            let notified = done.notified();
            if exit.lock().await.is_none() {
                let _ = tokio::time::timeout(Duration::from_millis(wait), notified).await;
            }
        }
        let ((o, o_omit), (e, e_omit), x) = tokio::join!(
            async { out.lock().await.snapshot_with_omitted() },
            async { err.lock().await.snapshot_with_omitted() },
            async { *exit.lock().await }
        );
        Ok(format_job(
            id,
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
        let (kill, exit_ref, done_ref) = {
            let map = self
                .inner
                .lock()
                .map_err(|_| "job table poisoned".to_owned())?;
            let h = map.get(id).ok_or_else(|| format!("unknown job `{id}`"))?;
            (
                Arc::clone(&h.kill),
                Arc::clone(&h.exit),
                Arc::clone(&h.done),
            )
        };
        if exit_ref.lock().await.is_some() {
            let snap = self.poll(id, None, Some(0)).await?;
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
        done_ref.notify_waiters();
        let snap = self.poll(id, None, Some(0)).await?;
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
    done: Arc<Notify>,
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
    // Signal completion *after* `exit` is written so a poll that
    // subscribes before reading `exit` can never miss the wakeup: either it
    // sees `Some` and returns immediately, or it is already subscribed.
    done.notify_waiters();
}

#[allow(clippy::too_many_arguments)]
fn format_job(
    id: &str,
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
    // Status header always: this is the signal poll exists for. Streams
    // follow only when non-empty (quiet-success rule); the start call in
    // history already shows the command, so it is not repeated here.
    match exit {
        None => s.push_str(&format!(
            "[job {id}, running=true, age_ms={}]\n",
            age.as_millis()
        )),
        Some(x) => s.push_str(&format!(
            "[job {id}, running=false, exit={}, expired={}, stopped={}, age_ms={}]\n",
            x.code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_owned()),
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
    crate::output::push_stream_sections(&mut s, &out.text, &err.text);
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

    fn test_config() -> ShellConfig {
        ShellConfig::default()
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
            )
            .await
            .unwrap();
        let snap = mgr.poll(&id, None, Some(0)).await.unwrap();
        assert!(snap.contains("running=true"), "got: {snap}");
        let stopped = mgr.stop(&id).await.unwrap();
        assert!(
            stopped.contains("stopped") || stopped.contains("running=false"),
            "{stopped}"
        );
        let snap2 = mgr.poll(&id, None, Some(0)).await.unwrap();
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
            )
            .await
            .unwrap();
        // Wait bounded for natural exit.
        let mut snap = String::new();
        for _ in 0..50 {
            snap = mgr.poll(&id, None, Some(0)).await.unwrap();
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
        assert!(mgr.poll("sh-999", None, Some(0)).await.is_err());
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
            )
            .await
            .unwrap();
        let mut snap = String::new();
        for _ in 0..50 {
            snap = mgr.poll(&id, None, Some(0)).await.unwrap();
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
        )
        .await
        .unwrap();
        assert_eq!(mgr.job_count(), 1);
        mgr.shutdown().await;
        assert_eq!(mgr.job_count(), 0);
    }

    #[tokio::test]
    async fn poll_omits_empty_sections_and_cmdline() {
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
            )
            .await
            .unwrap();
        let snap = mgr.poll(&id, None, Some(0)).await.unwrap();
        // Status header always; no scaffolding for empty streams and no
        // command repeat (the start call in history already shows it).
        assert!(snap.contains("running=true"), "got: {snap}");
        assert!(!snap.contains("[stdout]"), "got: {snap}");
        assert!(!snap.contains("[stderr]"), "got: {snap}");
        assert!(!snap.contains("cmd:"), "got: {snap}");
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn poll_shows_lone_stream_without_header() {
        let cfg = test_config();
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        let id = mgr
            .start(
                &cfg,
                "echo".to_owned(),
                vec!["echo".to_owned(), "job-out".to_owned()],
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let mut snap = String::new();
        for _ in 0..50 {
            snap = mgr.poll(&id, None, Some(0)).await.unwrap();
            if snap.contains("running=false") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(snap.contains("running=false"), "got: {snap}");
        assert!(snap.contains("job-out"), "got: {snap}");
        assert!(
            !snap.contains("[stdout]"),
            "lone stream needs no label: {snap}"
        );
        assert!(!snap.contains("[stderr]"), "got: {snap}");
        assert!(!snap.contains("cmd:"), "got: {snap}");
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn poll_waits_for_quick_exit_and_returns_early() {
        let cfg = test_config();
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        let id = mgr
            .start(
                &cfg,
                "echo".to_owned(),
                vec!["echo".to_owned(), "early".to_owned()],
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let start = Instant::now();
        let snap = mgr.poll(&id, None, Some(5_000)).await.unwrap();
        assert!(snap.contains("running=false"), "got: {snap}");
        assert!(snap.contains("early"), "got: {snap}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "early exit must not wait the full deadline"
        );
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn poll_times_out_on_running_job_but_stays_bounded() {
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
            )
            .await
            .unwrap();
        let start = Instant::now();
        let snap = mgr.poll(&id, None, Some(300)).await.unwrap();
        let elapsed = start.elapsed();
        assert!(snap.contains("running=true"), "got: {snap}");
        assert!(
            elapsed >= Duration::from_millis(200),
            "waited less than requested: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "wait exceeded its bound: {elapsed:?}"
        );
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn poll_zero_wait_is_instant() {
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
            )
            .await
            .unwrap();
        let start = Instant::now();
        let snap = mgr.poll(&id, None, Some(0)).await.unwrap();
        assert!(snap.contains("running=true"), "got: {snap}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "instant poll blocked: {:?}",
            start.elapsed()
        );
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn poll_default_waits_without_explicit_wait() {
        let cfg = test_config();
        let mgr = JobManager::new(JobLimits::from_config(&cfg));
        let id = mgr
            .start(
                &cfg,
                "echo".to_owned(),
                vec!["echo".to_owned(), "default-wait".to_owned()],
                None,
                None,
                None,
            )
            .await
            .unwrap();
        // `None` must behave like the ~2s default: quick exits resolve in one
        // call instead of needing a caller poll loop.
        let snap = mgr.poll(&id, None, None).await.unwrap();
        assert!(snap.contains("running=false"), "got: {snap}");
        assert!(snap.contains("default-wait"), "got: {snap}");
        mgr.shutdown().await;
    }
}
