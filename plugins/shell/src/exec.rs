//! Synchronous execution: direct spawn, bounded pipe drain, timeout kill.
//!
//! No policy guardrails here: any `argv` runs as-is. Memory is bounded by
//! `max_capture_bytes` per stream regardless of child output volume; wall
//! time is bounded by the clamped timeout. Non-zero exits are `Ok` (visible
//! to the model); spawn failures are `Err`. Timeouts return `Ok` with
//! `timed_out=true` plus the partial tail.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

use crate::env::apply_env;
use crate::output::format_shell_result;
use harness_config::ShellConfig;

/// Validated request for one synchronous run.
pub struct ExecRequest {
    pub argv: Vec<String>,
    pub exe: String,
    pub workdir: PathBuf,
    pub timeout: Duration,
    pub explicit_env: Option<HashMap<String, String>>,
}

/// Resolve + clamp a sync request. Pure except cwd/workdir canonicalization.
pub fn resolve_request(
    cfg: &ShellConfig,
    exe: String,
    argv: Vec<String>,
    workdir: Option<String>,
    timeout_ms: Option<u64>,
    explicit_env: Option<HashMap<String, String>>,
) -> Result<ExecRequest, String> {
    let workdir = resolve_workdir(workdir.as_deref())?;
    let want = timeout_ms.unwrap_or(cfg.default_timeout_ms);
    let clamped = want.clamp(1, cfg.max_timeout_ms.max(1));
    Ok(ExecRequest {
        argv,
        exe,
        workdir,
        timeout: Duration::from_millis(clamped),
        explicit_env,
    })
}

/// Resolve `workdir` (or cwd). No allowlist: any existing directory is fine.
/// Guardrails on *where* a command may run belong in a future guardrail
/// plugin via the `tool.approval` waterfall.
pub fn resolve_workdir(workdir: Option<&str>) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    let cwd_canon = canonical_or(&cwd);
    let target = match workdir {
        None => cwd_canon,
        Some(w) if w.trim().is_empty() => cwd_canon,
        Some(w) => {
            let p = Path::new(w);
            let joined = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            let canon = canonical_or(&joined);
            // Must exist and be a directory.
            if !canon.is_dir() {
                return Err(format!(
                    "workdir `{w}` does not exist or is not a directory"
                ));
            }
            canon
        }
    };
    Ok(target)
}

fn canonical_or(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Bounded tail ring shared between a reader task and the waiter.
#[derive(Debug, Default)]
pub(crate) struct Ring {
    buf: VecDeque<u8>,
    omitted: u64,
}

impl Ring {
    pub(crate) fn push(&mut self, chunk: &[u8], cap: usize) {
        let total = self.buf.len() + chunk.len();
        if total > cap {
            let drop_n = total - cap;
            for _ in 0..drop_n {
                self.buf.pop_front();
            }
            self.omitted += drop_n as u64;
        }
        self.buf.extend(chunk);
    }

    pub(crate) fn snapshot_with_omitted(&self) -> (Vec<u8>, u64) {
        (self.buf.iter().copied().collect(), self.omitted)
    }
}

pub(crate) async fn drain_into<T>(mut pipe: T, ring: Arc<Mutex<Ring>>, cap: usize)
where
    T: AsyncReadExt + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                ring.lock().await.push(&buf[..n], cap);
            }
            Err(_) => break,
        }
    }
}

/// Run one command synchronously. Returns labeled output (tail-truncated).
pub async fn run_once(cfg: &ShellConfig, req: ExecRequest) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(&req.exe);
    if req.argv.len() > 1 {
        cmd.args(&req.argv[1..]);
    }
    cmd.current_dir(&req.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_env(cmd.as_std_mut(), req.explicit_env.as_ref());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn `{}`: {e}", req.exe))?;
    let cap = cfg.max_capture_bytes.max(1024);
    let out_ring: Arc<Mutex<Ring>> = Arc::new(Mutex::new(Ring::default()));
    let err_ring: Arc<Mutex<Ring>> = Arc::new(Mutex::new(Ring::default()));

    let mut out_reader = None;
    let mut err_reader = None;
    if let Some(p) = child.stdout.take() {
        out_reader = Some(tokio::spawn(drain_into(p, Arc::clone(&out_ring), cap)));
    }
    if let Some(p) = child.stderr.take() {
        err_reader = Some(tokio::spawn(drain_into(p, Arc::clone(&err_ring), cap)));
    }

    let timed_out: bool;
    let exit_code: Option<i32>;
    match tokio::time::timeout(req.timeout, child.wait()).await {
        Ok(status) => {
            let status = status.map_err(|e| format!("wait failed: {e}"))?;
            timed_out = false;
            exit_code = status.code();
        }
        Err(_) => {
            // Timeout: kill, reap (grace 2s), partial tails survive in rings.
            timed_out = true;
            exit_code = None;
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
    }
    // Readers see EOF after kill/exit; join briefly then detach-reap.
    for h in [out_reader, err_reader].into_iter().flatten() {
        let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
    }
    let ((out, out_omit), (err, err_omit)) = tokio::join!(
        async { out_ring.lock().await.snapshot_with_omitted() },
        async { err_ring.lock().await.snapshot_with_omitted() }
    );
    let stdout = out;
    let mut stderr = err;
    if timed_out {
        let note = format!(
            "\n[timeout after {}ms; process killed]\n",
            req.timeout.as_millis()
        );
        push_bounded(&mut stderr, note.as_bytes(), cap);
    }
    Ok(format_shell_result(
        exit_code,
        timed_out,
        &stdout,
        &stderr,
        cfg.max_output_bytes,
        out_omit,
        err_omit,
    ))
}

fn push_bounded(dst: &mut Vec<u8>, extra: &[u8], cap: usize) {
    if dst.len() + extra.len() > cap {
        let drop_n = (dst.len() + extra.len() - cap).min(dst.len());
        dst.drain(..drop_n);
    }
    dst.extend_from_slice(extra);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workdir_defaults_to_cwd() {
        let wd = resolve_workdir(None).unwrap();
        assert!(wd.is_dir());
    }

    #[test]
    fn workdir_missing_is_error() {
        let err = resolve_workdir(Some("/definitely/not/allowed-xyz")).unwrap_err();
        assert!(err.contains("does not exist"));
    }

    #[test]
    fn timeout_clamped_to_max() {
        let cfg = ShellConfig::default();
        let req = resolve_request(
            &cfg,
            "echo".to_owned(),
            vec!["echo".to_owned()],
            None,
            Some(u64::MAX),
            None,
        )
        .unwrap();
        assert_eq!(req.timeout, Duration::from_millis(cfg.max_timeout_ms));
    }

    #[tokio::test]
    async fn sync_echo_returns_output_quietly() {
        let cfg = ShellConfig::default();
        let req = resolve_request(
            &cfg,
            "echo".to_owned(),
            vec!["echo".to_owned(), "hello".to_owned()],
            None,
            Some(5_000),
            None,
        )
        .unwrap();
        let out = run_once(&cfg, req).await.unwrap();
        assert_eq!(out, "hello\n");
    }

    #[tokio::test]
    async fn sync_timeout_reports_timed_out() {
        let cfg = ShellConfig::default();
        let req = resolve_request(
            &cfg,
            "python3".to_owned(),
            vec![
                "python3".to_owned(),
                "-c".to_owned(),
                "import time; time.sleep(30)".to_owned(),
            ],
            None,
            Some(300),
            None,
        )
        .unwrap();
        let out = run_once(&cfg, req).await.unwrap();
        assert!(out.contains("timed_out=true"), "got: {out}");
    }

    #[tokio::test]
    async fn huge_output_is_bounded() {
        let cfg = ShellConfig::default();
        let req = resolve_request(
            &cfg,
            "python3".to_owned(),
            vec![
                "python3".to_owned(),
                "-c".to_owned(),
                "for i in range(200000): print('x'*80)".to_owned(),
            ],
            None,
            Some(20_000),
            None,
        )
        .unwrap();
        let out = run_once(&cfg, req).await.unwrap();
        // Labeled output must stay well under raw 16MB.
        assert!(
            out.len() < cfg.max_output_bytes * 2 + 1024,
            "len={}",
            out.len()
        );
        assert!(out.contains("truncated=true"));
    }
}
