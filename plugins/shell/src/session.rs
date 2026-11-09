//! Persistent bash session: one long-lived `bash` process per service.
//!
//! Every `shell_exec` call (without an explicit `env`) runs inside this
//! session, so shell state persists across calls: `cd`, `export`ed
//! variables, functions, and files are still there for the next command —
//! the same model-visible behavior as Claude Code's bash tool.
//!
//! Protocol per call: the script runs grouped with stdin from `/dev/null`
//! (so commands like bare `cat` get EOF instead of eating the command
//! pipe) and stderr to a per-call temp file; a nonce marker line carrying
//! the exit code terminates the read. Calls serialize on a mutex. A
//! timeout kills and respawns the shell (partial tails survive); a dead
//! shell is respawned transparently once, so builtins like `exit` reset
//! state instead of wedging the service.
//!
//! An explicit `workdir` runs inside `(cd dir && …)` so the session's own
//! directory is untouched; without one, `cd` persists by design.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::env::{detach_tty, kill_process_group};
use crate::exec::Ring;

/// Process-wide nonce counter: temp files and markers must be unique
/// across sessions too, since tests (and turns) run sessions in parallel
/// in one process. Combined with pid + per-session seq below.
static NONCE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Finished session run: raw stream tails plus omission counts for the
/// caller to format.
#[derive(Debug)]
pub struct SessionOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub out_omitted: u64,
    pub err_omitted: u64,
}

enum ShellProcess {
    Root(Child),
    Fork {
        pid: u32,
        directory: std::path::PathBuf,
    },
}

impl ShellProcess {
    fn id(&self) -> Option<u32> {
        match self {
            Self::Root(child) => child.id(),
            Self::Fork { pid, .. } => Some(*pid),
        }
    }
    fn start_kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Root(child) => child.start_kill(),
            Self::Fork { pid, .. } => {
                kill_process_group(Some(*pid));
                Ok(())
            }
        }
    }
    fn alive(&mut self) -> bool {
        match self {
            Self::Root(child) => matches!(child.try_wait(), Ok(None)),
            Self::Fork { pid, .. } => unsafe { libc::kill(*pid as i32, 0) == 0 },
        }
    }
}
impl Drop for ShellProcess {
    fn drop(&mut self) {
        kill_process_group(self.id());
        if let Self::Fork { directory, .. } = self {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

struct SessionState {
    child: ShellProcess,
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    stdout: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    seq: u64,
    in_flight: bool,
}

pub struct BashSession {
    state: Mutex<Option<SessionState>>,
}

impl BashSession {
    pub fn new() -> Self {
        BashSession {
            state: Mutex::new(None),
        }
    }

    /// Graceful shutdown: kill the session shell, if any.
    pub async fn shutdown(&self) {
        let mut guard = self.state.lock().await;
        if let Some(mut state) = guard.take() {
            kill_process_group(state.child.id());
            let _ = state.child.start_kill();
            if let ShellProcess::Root(child) = &mut state.child {
                let _ = child.wait().await;
            }
        }
    }

    fn spawn() -> std::io::Result<SessionState> {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["--noprofile", "--norc", "-s"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        detach_tty(&mut cmd);
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        Ok(SessionState {
            child: ShellProcess::Root(child),
            stdin: Box::new(stdin),
            stdout: BufReader::new(Box::new(stdout)),
            seq: 0,
            in_flight: false,
        })
    }

    /// Fork Bash itself, without exec or serialization of its variables.
    pub async fn fork(&self, timeout: Duration) -> Result<Self, String> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let directory = std::env::temp_dir().join(format!(
            "harness-fork-{}-{}",
            std::process::id(),
            NONCE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let input = directory.join("in");
        let output = directory.join("out");
        for path in [&input, &output] {
            let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                .map_err(|e| e.to_string())?;
            if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        let open = |path: &Path| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_CLOEXEC)
                .open(path)
                .map_err(|e| e.to_string())
        };
        let writer = open(&input)?;
        let reader = open(&output)?;
        // Job control gives the fork an independent process group. Restore the
        // parent's monitor setting before returning to its command protocol.
        let script = format!(
            r#"case $- in *m*) __harness_monitor=1;; *) __harness_monitor=0; set -m;; esac
( if [[ $__harness_monitor == 0 ]]; then set +m; fi; unset __harness_monitor; builtin printf '%s\n' "$BASHPID"; while IFS= builtin read -r __harness_line; do builtin eval "$__harness_line"; done ) <{} >{} 2>/dev/null &
if [[ $__harness_monitor == 0 ]]; then set +m; fi
unset __harness_monitor"#,
            sh_quote(&input.to_string_lossy()),
            sh_quote(&output.to_string_lossy())
        );
        let out = self.run(&script, None, timeout, 65536).await?;
        if out.timed_out || out.exit_code != Some(0) {
            let _ = std::fs::remove_dir_all(&directory);
            return Err("could not fork parent shell".into());
        }
        let mut stdout: BufReader<Box<dyn AsyncRead + Send + Unpin>> =
            BufReader::new(Box::new(tokio::fs::File::from_std(reader)));
        let mut pid = String::new();
        tokio::time::timeout(timeout, stdout.read_line(&mut pid))
            .await
            .map_err(|_| "child shell handshake timed out".to_owned())?
            .map_err(|e| e.to_string())?;
        let pid = pid.trim().parse::<u32>().map_err(|e| e.to_string())?;
        Ok(Self {
            state: Mutex::new(Some(SessionState {
                child: ShellProcess::Fork { pid, directory },
                stdin: Box::new(tokio::fs::File::from_std(writer)),
                stdout,
                seq: 0,
                in_flight: false,
            })),
        })
    }

    /// Runs one shell command string in the session. `workdir` scopes just
    /// this call; `None` runs in the session's current directory.
    pub async fn run(
        &self,
        script: &str,
        workdir: Option<&Path>,
        timeout: Duration,
        cap: usize,
    ) -> Result<SessionOutput, String> {
        let mut guard = self.state.lock().await;
        // A dropped run future can leave unread output and a live command in
        // the protocol stream. Never reuse that shell on the next call.
        if guard.as_ref().is_some_and(|state| state.in_flight) {
            Self::kill_slot(&mut guard);
        }
        for _ in 0..2 {
            if !Self::alive(&mut guard) {
                *guard = Some(Self::spawn().map_err(|e| format!("cannot start bash: {e}"))?);
            }
            let state = guard.as_mut().expect("session spawned");
            state.seq += 1;
            let nonce = format!(
                "{}-{}-{}",
                std::process::id(),
                NONCE_SEQ.fetch_add(1, Ordering::Relaxed),
                state.seq
            );
            let tmp = std::env::temp_dir().join(format!("harness-sh-{nonce}.err"));
            let mut payload = build_payload(script, workdir, &tmp, &nonce);
            if matches!(state.child, ShellProcess::Fork { .. }) {
                // One line per request; Bash decodes bytes without invoking a
                // fresh interpreter, preserving unexported state across calls.
                let encoded: String = payload.bytes().map(|b| format!("\\x{b:02x}")).collect();
                payload = format!("builtin eval $'{encoded}'");
            }
            state.in_flight = true;
            match Self::attempt(state, &payload, &nonce, &tmp, timeout, cap).await {
                Attempt::Done(out) => {
                    state.in_flight = false;
                    return Ok(out);
                }
                Attempt::TimedOut(out) => {
                    Self::kill_slot(&mut guard);
                    return Ok(out);
                }
                // Broken pipe/read with no clean exit: respawn once and
                // retry; a second breakage surfaces as a tool error.
                Attempt::Broken => {
                    Self::kill_slot(&mut guard);
                }
            }
        }
        Err("bash session ended unexpectedly and could not be restarted".to_owned())
    }

    /// True when a live shell is present. A previously exited shell is
    /// dropped so the caller respawns.
    fn alive(slot: &mut Option<SessionState>) -> bool {
        let Some(state) = slot.as_mut() else {
            return false;
        };
        match state.child.alive() {
            true => true,
            false => {
                *slot = None;
                false
            }
        }
    }

    /// Kill and drop the current shell; the next loop iteration respawns.
    /// Kill errors are ignored (already-dead child).
    fn kill_slot(slot: &mut Option<SessionState>) {
        if let Some(mut state) = slot.take() {
            kill_process_group(state.child.id());
            let _ = state.child.start_kill();
        }
    }

    async fn attempt(
        state: &mut SessionState,
        payload: &str,
        nonce: &str,
        tmp: &Path,
        timeout: Duration,
        cap: usize,
    ) -> Attempt {
        if state.stdin.write_all(payload.as_bytes()).await.is_err()
            || state.stdin.write_all(b"\n").await.is_err()
            || state.stdin.flush().await.is_err()
        {
            let _ = tokio::fs::remove_file(tmp).await;
            return Attempt::Broken;
        }
        let marker = format!("__HARNESS_DONE_{nonce}");
        match tokio::time::timeout(timeout, read_until_marker(&mut state.stdout, &marker, cap))
            .await
        {
            Ok(ReadOutcome::Done {
                out,
                out_omit,
                exit,
            }) => {
                let (err, err_omit) = read_capped_tail(tmp, cap).await;
                let _ = tokio::fs::remove_file(tmp).await;
                Attempt::Done(SessionOutput {
                    exit_code: exit,
                    timed_out: false,
                    stdout: out,
                    stderr: err,
                    out_omitted: out_omit,
                    err_omitted: err_omit,
                })
            }
            // The script ended the shell itself (`exit N`): report its
            // code like any completed run, with whatever output and
            // stderr arrived. The session transparently respawns next
            // call with reset state.
            Ok(ReadOutcome::Eof { out, out_omit }) => {
                // EOF proves the shell closed stdout, so it is exiting or
                // exited and `wait` reaps promptly. (`try_wait` is wrong
                // here: under load the kernel can deliver pipe EOF before
                // the exiting process becomes waitable — a close-vs-exit
                // race that flaked every `exit`-involving test.)
                let code = match &mut state.child {
                    ShellProcess::Root(child) => child.wait().await.ok().and_then(|s| s.code()),
                    _ => None,
                };
                // Reap so no zombie lingers; the slot is dropped by the
                // caller path (respawn happens on the next run).
                let (err, err_omit) = read_capped_tail(tmp, cap).await;
                let _ = tokio::fs::remove_file(tmp).await;
                match code {
                    Some(c) => Attempt::Done(SessionOutput {
                        exit_code: Some(c),
                        timed_out: false,
                        stdout: out,
                        stderr: err,
                        out_omitted: out_omit,
                        err_omitted: err_omit,
                    }),
                    // Signal/crash with no status: retry once on a fresh
                    // shell rather than attributing mystery output.
                    None => Attempt::Broken,
                }
            }
            Ok(ReadOutcome::Broken) => {
                let _ = tokio::fs::remove_file(tmp).await;
                Attempt::Broken
            }
            Err(_) => {
                // Timeout: partial stdout tail survives; stderr tail too.
                // The shell is killed by the caller (`kill_slot`).
                let (out, out_omit) = take_partial_stdout(&mut state.stdout, cap).await;
                let (err, err_omit) = read_capped_tail(tmp, cap).await;
                let _ = tokio::fs::remove_file(tmp).await;
                Attempt::TimedOut(SessionOutput {
                    exit_code: None,
                    timed_out: true,
                    stdout: out,
                    stderr: err,
                    out_omitted: out_omit,
                    err_omitted: err_omit,
                })
            }
        }
    }
}

enum Attempt {
    Done(SessionOutput),
    TimedOut(SessionOutput),
    Broken,
}

enum ReadOutcome {
    Done {
        out: Vec<u8>,
        out_omit: u64,
        exit: Option<i32>,
    },
    /// Shell EOF before the marker: carries the stdout tail collected so
    /// far; the caller resolves the exit via `try_wait`.
    Eof { out: Vec<u8>, out_omit: u64 },
    /// Read error mid-stream (no EOF, no marker).
    Broken,
}

/// Reads stdout lines until the marker line, keeping a bounded tail ring.
/// The marker line's pre-marker bytes belong to the command (missing
/// trailing newline case); the exit code rides on the marker line.
async fn read_until_marker(
    stdout: &mut BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    marker: &str,
    cap: usize,
) -> ReadOutcome {
    let mut ring = Ring::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        match stdout.read_until(b'\n', &mut line).await {
            Ok(0) => {
                let (out, omit) = ring.snapshot_with_omitted();
                return ReadOutcome::Eof {
                    out,
                    out_omit: omit,
                };
            }
            Ok(_) => {}
            Err(_) => return ReadOutcome::Broken,
        }
        if let Some(pos) = find_subslice(&line, marker.as_bytes()) {
            let exit = parse_exit(&line, marker);
            // The exit tag rides the same echo line ahead of the marker;
            // cut there so neither leaks into command output.
            let cut = find_subslice(&line, b"__HARNESS_EXIT:")
                .filter(|&tag| tag <= pos)
                .unwrap_or(pos);
            ring.push(&line[..cut], cap);
            let (out, omit) = ring.snapshot_with_omitted();
            return ReadOutcome::Done {
                out,
                out_omit: omit,
                exit,
            };
        }
        ring.push(&line, cap);
    }
}

/// Best-effort drain of whatever stdout is already buffered (post-timeout).
/// Reads with a short grace window; EOF or silence ends it.
async fn take_partial_stdout(
    stdout: &mut BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    cap: usize,
) -> (Vec<u8>, u64) {
    let mut ring = Ring::default();
    let mut buf = [0u8; 8192];
    loop {
        match tokio::time::timeout(Duration::from_millis(100), stdout.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => ring.push(&buf[..n], cap),
        }
    }
    ring.snapshot_with_omitted()
}

/// Reads the per-call stderr file's tail, bounded by `cap`.
async fn read_capped_tail(path: &Path, cap: usize) -> (Vec<u8>, u64) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let cap = cap.max(1024);
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return (Vec::new(), 0);
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    // Over-read slightly so the UTF-8 boundary fixup never eats content.
    let start = len.saturating_sub(cap as u64 + 4);
    let mut buf = Vec::new();
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err()
        || file.read_to_end(&mut buf).await.is_err()
    {
        return (Vec::new(), 0);
    }
    let tail = crate::output::tail_truncate(&buf, cap);
    (tail.text.into_bytes(), start + tail.omitted_bytes)
}

/// Builds the wrapper sent to the session shell. Grouped in `{ }` so the
/// redirections cover multi-command scripts; subshelled only for an
/// explicit `workdir` so bare `cd` persists by design.
fn build_payload(script: &str, workdir: Option<&Path>, tmp: &Path, nonce: &str) -> String {
    let marker = format!("__HARNESS_DONE_{nonce}");
    let body = match workdir {
        Some(dir) => format!("(cd {} &&\n{}\n)", sh_quote(&dir.to_string_lossy()), script),
        None => format!("{{\n{}\n}}", script),
    };
    format!(
        "{body} 2>{tmp} </dev/null; __c=$?; echo \"__HARNESS_EXIT:${{__c}} {marker}\"",
        tmp = sh_quote(&tmp.to_string_lossy()),
    )
}

/// Single-quote a path for shell embedding.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_exit(line: &[u8], marker: &str) -> Option<i32> {
    let text = String::from_utf8_lossy(line);
    let tag = "__HARNESS_EXIT:";
    let start = text.find(tag)? + tag.len();
    // The exit digits precede the marker on the same echo line.
    let end = text.find(marker)?;
    text[start..end].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(session: &BashSession, script: &str) -> SessionOutput {
        session
            .run(script, None, Duration::from_secs(10), 65536)
            .await
            .unwrap()
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(sh_quote("o'clock"), "'o'\\''clock'");
    }

    #[tokio::test]
    async fn live_fork_preserves_nonexported_arrays_aliases_and_functions() {
        let parent = BashSession::new();
        let timeout = Duration::from_secs(5);
        let initialized = parent.run(r#"secret='not exported'; declare -A mapping=([key]='array value'); alias inherited_alias='printf alias-value'; inherited_fn() { printf '%s' "$secret"; }; shopt -s nullglob; umask 027"#, None, timeout, 65536).await.unwrap();
        assert_eq!(initialized.exit_code, Some(0));
        let child = parent.fork(timeout).await.unwrap();
        let result = child.run(r#"printf '%s|%s|' "$secret" "${mapping[key]}"; inherited_fn; alias inherited_alias; shopt -q nullglob; printf '|%s' "$?"; umask; secret=changed"#, None, timeout, 65536).await.unwrap();
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(
            output.contains("not exported|array value|not exported"),
            "{output}"
        );
        assert!(output.contains("alias-value"), "{output}");
        assert!(output.contains("|00027"), "{output}");
        let parent_result = parent
            .run(r#"printf '%s' "$secret""#, None, timeout, 65536)
            .await
            .unwrap();
        assert_eq!(parent_result.stdout, b"not exported");
        let grandchild = child.fork(timeout).await.unwrap();
        let result = grandchild
            .run(r#"printf '%s' "$secret""#, None, timeout, 65536)
            .await
            .unwrap();
        assert_eq!(result.stdout, b"changed");
        grandchild.shutdown().await;
        child.shutdown().await;
        parent.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_command_does_not_corrupt_next_protocol_response() {
        let session = std::sync::Arc::new(BashSession::new());
        let worker = session.clone();
        let task = tokio::spawn(async move {
            worker
                .run("sleep 5; echo stale", None, Duration::from_secs(10), 65536)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        let _ = task.await;
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            session.run("echo fresh", None, Duration::from_secs(1), 65536),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(out.stdout, b"fresh\n");
        session.shutdown().await;
    }

    #[tokio::test]
    async fn simple_command_returns_output_and_zero_exit() {
        let session = BashSession::new();
        let out = run(&session, "echo hello").await;
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
        assert!(out.stderr.is_empty());
    }

    #[tokio::test]
    async fn pipelines_and_chains_work() {
        // The exact shape that failed as argv: now a shell string.
        let session = BashSession::new();
        let out = run(
            &session,
            "pwd; ls -la | head -n 3; echo hi 2>&1 | tr a-z A-Z",
        )
        .await;
        assert_eq!(out.exit_code, Some(0));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("HI"), "got: {text}");
        assert!(text.contains("total"), "got: {text}");
    }

    #[tokio::test]
    async fn state_persists_across_runs() {
        let session = BashSession::new();
        run(&session, "export HARNESS_PROBE=yes").await;
        let out = run(&session, "echo $HARNESS_PROBE").await;
        assert_eq!(String::from_utf8_lossy(&out.stdout), "yes\n");

        run(&session, "cd /").await;
        let out = run(&session, "pwd").await;
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "/");
    }

    #[tokio::test]
    async fn workdir_scopes_one_call_without_mutating_session() {
        let session = BashSession::new();
        let start = run(&session, "pwd").await;
        let start = String::from_utf8_lossy(&start.stdout).trim().to_owned();
        let scoped = session
            .run("pwd", Some(Path::new("/")), Duration::from_secs(10), 65536)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&scoped.stdout).trim(), "/");
        let after = run(&session, "pwd").await;
        assert_eq!(String::from_utf8_lossy(&after.stdout).trim(), start);
    }

    #[tokio::test]
    async fn missing_workdir_is_a_command_failure_not_a_tool_error() {
        let session = BashSession::new();
        let out = session
            .run(
                "pwd",
                Some(Path::new("/definitely/not/allowed-xyz")),
                Duration::from_secs(10),
                65536,
            )
            .await
            .unwrap();
        assert!(!out.timed_out);
        assert_ne!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn stderr_is_captured_separately() {
        let session = BashSession::new();
        let out = run(&session, "echo out; echo err >&2; exit 3").await;
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    }

    #[tokio::test]
    async fn bare_cat_gets_eof_instead_of_hanging() {
        let session = BashSession::new();
        let out = session
            .run("cat", None, Duration::from_secs(10), 65536)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn timeout_kills_and_next_call_works() {
        let session = BashSession::new();
        let slow = session
            .run("sleep 30", None, Duration::from_millis(300), 65536)
            .await
            .unwrap();
        assert!(slow.timed_out, "got exit {:?}", slow.exit_code);
        let alive = run(&session, "echo alive").await;
        assert!(!alive.timed_out);
        assert_eq!(String::from_utf8_lossy(&alive.stdout), "alive\n");
    }

    #[tokio::test]
    async fn exit_builtin_reports_its_code_and_resets_session() {
        let session = BashSession::new();
        // `exit` ends the session shell, but its code and output are
        // reported like any completed run (not a tool error).
        let out = session
            .run(
                "echo out; echo err >&2; exit 3",
                None,
                Duration::from_secs(10),
                65536,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
        // State was reset: the probe export from a fresh shell is absent
        // and the service is usable.
        let out = session
            .run(
                "echo \"probe=${HARNESS_EXIT_PROBE:-absent}\"",
                None,
                Duration::from_secs(10),
                65536,
            )
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("probe=absent"));
    }

    #[tokio::test]
    async fn session_has_no_controlling_terminal() {
        // Fix A: the session shell runs setsid'd (new session, no ctty),
        // so `sudo`/`su`/`passwd` cannot open `/dev/tty` and paint over
        // the TUI input box. Prove it by comparing session ids: the child
        // shell must live in a different session than this test process.
        let session = BashSession::new();
        let out = run(&session, "ps -o sid= -p $$").await;
        assert_eq!(out.exit_code, Some(0));
        let child_sid: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("ps must print the shell sid");
        // SAFETY: getsid(0) queries our own session; cannot fail.
        let parent_sid = unsafe { libc::getsid(0) };
        assert_ne!(
            child_sid, parent_sid,
            "session shell must be setsid'd (child sid {child_sid} == parent sid {parent_sid})"
        );
        // And `/dev/tty` must not open from inside the session.
        let out = run(&session, r#"python3 -c 'open("/dev/tty")'"#).await;
        assert_ne!(out.exit_code, Some(0), "/dev/tty must not open in session");
    }

    #[tokio::test]
    async fn concurrent_runs_serialize_correctly() {
        use std::sync::Arc;
        let session = Arc::new(BashSession::new());
        let mut handles = Vec::new();
        for i in 0..8u32 {
            let s = session.clone();
            handles.push(tokio::spawn(async move {
                s.run(
                    &format!("echo start-{i}; sleep 0.05; echo end-{i}"),
                    None,
                    Duration::from_secs(10),
                    65536,
                )
                .await
                .unwrap()
            }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            let out = h.await.unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains(&format!("start-{i}")), "task {i}: {text}");
            assert!(text.contains(&format!("end-{i}")), "task {i}: {text}");
        }
    }
}
