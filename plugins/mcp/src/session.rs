//! One MCP stdio server: a persistent child process plus async plumbing.
//!
//! Layout mirrors `shell::jobs`: `start` spawns immediately, a reader task
//! owns stdout, writers serialize on stdin, cleanup is layered
//! (`kill_on_drop` on every child + `Drop`/`shutdown` aborting tasks, which
//! drops/kills the child). One session per configured server; calls
//! multiplex over it via `pending: HashMap<id, oneshot::Sender>`.
//!
//! No policy lives here: any configured command runs. Resource bounds only
//! (request timeout, stderr cap, output cap).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify, oneshot};

use crate::protocol::{
    CallResult, ContentPart, RpcError, RpcNotification, RpcRequest, RpcResponse, ToolInfo,
    ToolsListResult, initialize_params, is_notification, response_id,
};
use harness_contracts::McpServerConfig;

/// Stderr drain cap (ring). Servers that log verbosely must never grow us.
const STDERR_CAP: usize = 64 * 1024;

/// Guard time after stdin close before `kill_on_drop` reaps a hung server.
const SHUTDOWN_GRACE_MS: u64 = 500;

/// Bounded stderr ring: keeps the newest bytes, counts what fell off.
struct ErrRing {
    buf: Vec<u8>,
    omitted: u64,
}

impl ErrRing {
    fn new() -> Self {
        ErrRing {
            buf: Vec::new(),
            omitted: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > STDERR_CAP {
            let drop = self.buf.len() - STDERR_CAP;
            self.buf.drain(..drop);
            self.omitted += drop as u64;
        }
    }

    fn snapshot(&self) -> (Vec<u8>, u64) {
        (self.buf.clone(), self.omitted)
    }
}

/// Handle to one running MCP server.
pub struct McpSession {
    /// Advertised tools (refreshed on `tools/list_changed`).
    pub tools: Mutex<Vec<ToolInfo>>,
    per_call_timeout_ms: u64,
    max_output_bytes: usize,
    next_id: AtomicU64,
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
    stderr: Arc<Mutex<ErrRing>>,
    list_changed: Notify,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl McpSession {
    /// Spawn the server process and run the MCP handshake (`initialize` →
    /// `notifications/initialized`). Returns the live session; `tools/list`
    /// is a separate step so callers can parallelize across servers.
    pub async fn spawn(
        _server: String,
        cfg: &McpServerConfig,
        max_output_bytes: usize,
    ) -> Result<Arc<Self>, String> {
        if cfg.command.trim().is_empty() {
            return Err("mcp server command must not be empty".to_owned());
        }
        for k in cfg.env.keys() {
            if k.is_empty() || k.contains('=') || k.contains('\0') {
                return Err(format!("invalid env key `{k}`"));
            }
        }
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in cfg.env.iter() {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn `{}`: {e}", cfg.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "server stdin unavailable".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "server stdout unavailable".to_owned())?;
        let stderr = child.stderr.take();

        let sess = Arc::new(McpSession {
            tools: Mutex::new(Vec::new()),
            per_call_timeout_ms: cfg.effective_timeout_ms(),
            max_output_bytes: max_output_bytes.max(1024),
            next_id: AtomicU64::new(1),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            stderr: Arc::new(Mutex::new(ErrRing::new())),
            list_changed: Notify::new(),
            tasks: Mutex::new(Vec::new()),
        });

        // Reader + stderr + owner tasks share the session handle. Tasks are
        // registered before any call can run (still in `spawn`), so the
        // mutex is uncontended after setup.
        {
            let mut tasks = sess.tasks.lock().await;
            tasks.push(
                tokio::spawn(reader_task(Arc::clone(&sess), BufReader::new(stdout))).abort_handle(),
            );
            if let Some(p) = stderr {
                tasks.push(tokio::spawn(stderr_task(Arc::clone(&sess), p)).abort_handle());
            }
            // Owner task: holds the `Child` for the session lifetime;
            // `kill_on_drop` kills the server when it drops.
            tasks.push(tokio::spawn(owner_task(child)).abort_handle());
        }

        // Handshake with an overall bound so a hung server can't stall init.
        let hs = async {
            let _ = sess
                .request("initialize", initialize_params())
                .await
                .map_err(|e| format!("initialize: {e}"))?;
            sess.notify("notifications/initialized", serde_json::json!({}))
                .await?;
            Ok::<(), String>(())
        };
        match tokio::time::timeout(std::time::Duration::from_secs(15), hs).await {
            Ok(r) => r?,
            Err(_) => {
                let (tail, _) = sess.stderr_snapshot().await;
                return Err(format!(
                    "initialize timed out: {}",
                    String::from_utf8_lossy(&tail).trim()
                ));
            }
        }
        Ok(sess)
    }

    /// Full `tools/list` with cursor pagination. Stores the result on the
    /// session and returns a snapshot.
    pub async fn list_tools(self: &Arc<Self>) -> Result<Vec<ToolInfo>, String> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let v = self.request("tools/list", params).await.map_err(|e| e.to_string())?;
            let page: ToolsListResult = serde_json::from_value(v)
                .map_err(|e| format!("bad tools/list result: {e}"))?;
            out.extend(page.tools);
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        *self.tools.lock().await = out.clone();
        Ok(out)
    }

    /// One `tools/call`. Returns rendered text or a `ToolError`-shaped
    /// string (`Err` carries `tool: message` rendering for the caller).
    pub async fn call_tool(&self, tool: &str, args: Value) -> Result<String, String> {
        let params = serde_json::json!({ "name": tool, "arguments": args });
        let v = self
            .request("tools/call", params)
            .await
            .map_err(|e| e.to_string())?;
        let res: CallResult =
            serde_json::from_value(v).map_err(|e| format!("bad tools/call result: {e}"))?;
        render_call_result(tool, &res, self.max_output_bytes)
    }

    /// Wait for a `notifications/tools/list_changed` from this server.
    pub async fn changed_notified(&self) {
        self.list_changed.notified().await;
    }

    /// Latest stderr tail (for spawn/handshake diagnostics).
    pub async fn stderr_snapshot(&self) -> (Vec<u8>, u64) {
        self.stderr.lock().await.snapshot()
    }

    /// Core request path: allocate id, register waiter, write one line,
    /// await with per-call timeout. Concurrent-safe; stdin writes are
    /// serialized by the mutex.
    async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = RpcRequest::new(id, method, params);
        let mut line = serde_json::to_string(&req)
            .map_err(|e| RpcError::internal(format!("encode request: {e}")))?;
        line.push('\n');

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let write = async {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("write to server stdin: {e}"))?;
            stdin
                .flush()
                .await
                .map_err(|e| format!("flush server stdin: {e}"))?;
            Ok::<(), String>(())
        };
        if let Err(e) = write.await {
            self.pending.lock().await.remove(&id);
            return Err(RpcError::internal(e));
        }

        let wait = tokio::time::timeout(
            std::time::Duration::from_millis(self.per_call_timeout_ms),
            rx,
        )
        .await;
        match wait {
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::internal(format!(
                    "`{method}` timed out after {}ms",
                    self.per_call_timeout_ms
                )))
            }
            Ok(Err(_)) => Err(RpcError::internal("server disconnected")),
            Ok(Ok(r)) => r,
        }
    }

    /// Fire-and-forget notification (init handshake only).
    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let n = RpcNotification::new(method, params);
        let mut line = serde_json::to_string(&n).map_err(|e| format!("encode: {e}"))?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write to server stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("flush server stdin: {e}"))?;
        Ok(())
    }

    /// Graceful shutdown: close stdin (EOF), wait briefly, then abort
    /// tasks (abort drops the waiter/child; `kill_on_drop` kills it).
    pub async fn shutdown(&self) {
        // Closing stdin signals EOF to well-behaved servers. Take the
        // lock, drop the handle via shutdown of the pipe.
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.shutdown().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(SHUTDOWN_GRACE_MS)).await;
        for t in self.tasks.lock().await.iter() {
            t.abort();
        }
        // Fail every in-flight waiter so tool handlers return promptly.
        let mut pending = self.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(RpcError::internal("server shut down")));
        }
    }

    /// Abort every task. Sync so `Drop` can call it; aborting the owner
    /// drops its owned `Child`, and `kill_on_drop(true)` kills the server.
    fn abort_all_sync(&self) {
        if let Ok(tasks) = self.tasks.try_lock() {
            for t in tasks.iter() {
                t.abort();
            }
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.abort_all_sync();
    }
}

/// Stdout reader: newline-delimited JSON. Responses route to the matching
/// waiter by id; notifications arm `list_changed` (or are ignored);
/// anything else is fail-quiet. EOF fails all waiters.
async fn reader_task(sess: Arc<McpSession>, stdout: BufReader<tokio::process::ChildStdout>) {
    let mut lines = stdout.lines();
    loop {
        match lines.next_line().await {
            Err(_) | Ok(None) => break, // EOF / pipe error: fail waiters below.
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue, // Non-JSON log line on stdout: skip.
                };
                if is_notification(&v) {
                    if v.get("method").and_then(Value::as_str)
                        == Some("notifications/tools/list_changed")
                    {
                        sess.list_changed.notify_waiters();
                    }
                    continue;
                }
                let Some(id) = response_id(&v) else { continue };
                let parsed: Result<RpcResponse, _> = serde_json::from_value(v);
                let msg: Result<Value, RpcError> = match parsed {
                    Err(e) => Err(RpcError::internal(format!("bad response: {e}"))),
                    Ok(r) => match (r.result, r.error) {
                        (Some(val), None) => Ok(val),
                        (None, Some(err)) => Err(err),
                        _ => Err(RpcError::internal("response has neither result nor error")),
                    },
                };
                if let Some(tx) = sess.pending.lock().await.remove(&id) {
                    let _ = tx.send(msg);
                }
            }
        }
    }
    // EOF: wake every waiter with a disconnect so handlers return.
    let mut pending = sess.pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(RpcError::internal("server disconnected")));
    }
}

/// Stderr drain:Servers may log; bound it in a ring and never block.
async fn stderr_task(sess: Arc<McpSession>, stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncReadExt;
    let mut err = stderr;
    let mut chunk = [0u8; 4096];
    loop {
        match err.read(&mut chunk).await {
            Err(_) | Ok(0) => break,
            Ok(n) => sess.stderr.lock().await.push(&chunk[..n]),
        }
    }
}

/// Owns the `Child` for the session lifetime: reaps it promptly on
/// natural exit, kills it on teardown (`kill_on_drop` fires when this task
/// is aborted and `child` drops).
async fn owner_task(mut child: tokio::process::Child) {
    let _ = child.wait().await;
}

/// Renders a `tools/call` result to model-facing text. `isError: true`
/// still returns `Ok` text (the tool ran; the payload says what happened)
/// — only transport/protocol failures are `Err`.
fn render_call_result(tool: &str, res: &CallResult, cap: usize) -> Result<String, String> {
    let mut parts: Vec<String> = Vec::new();
    let mut omitted_kinds = 0usize;
    for p in &res.content {
        // `text` parts (and schemaless parts with text) render; typed
        // non-text parts (image blobs, resources, …) are counted out.
        if p.kind == "text" || p.kind.is_empty() {
            if let Some(t) = &p.text {
                parts.push(t.clone());
            }
        } else {
            omitted_kinds += 1;
        }
    }
    let mut text = parts.join("\n");
    if omitted_kinds > 0 {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[{omitted_kinds} non-text content part(s) omitted]"));
    }
    if text.is_empty() {
        text = "(empty result)".to_owned();
    }
    if res.is_error == Some(true) {
        // Surface as an error result, not a transport failure: the loop
        // records it via `tool.executed` like any other `ToolError`.
        return Err(format!("tool `{tool}` returned an error: {text}"));
    }
    if text.len() > cap {
        let cut = text.floor_char_boundary(cap);
        text.truncate(cut);
        text.push_str(&format!("\n[truncated at {cap}B]"));
    }
    Ok(text)
}

/// Exposed for unit tests: renders content parts to text.
#[allow(dead_code)]
pub fn render_for_test(content: &[ContentPart], is_error: Option<bool>, cap: usize) -> Result<String, String> {
    render_call_result(
        "test",
        &CallResult {
            content: content.to_vec(),
            is_error,
        },
        cap,
    )
}


#[cfg(test)]
mod tests {
    use super::*;

    fn part(text: &str) -> ContentPart {
        ContentPart {
            kind: "text".to_owned(),
            text: Some(text.to_owned()),
        }
    }

    #[test]
    fn render_joins_text_parts() {
        let out = render_for_test(&[part("a"), part("b")], None, 1024).unwrap();
        assert_eq!(out, "a\nb");
    }

    #[test]
    fn render_marks_non_text_parts() {
        let blob = ContentPart {
            kind: "image".to_owned(),
            text: None,
        };
        let out = render_for_test(&[part("hi"), blob], None, 1024).unwrap();
        assert!(out.contains("hi"), "{out:?}");
        assert!(out.contains("non-text"), "{out:?}");
    }

    #[test]
    fn render_empty_result_has_placeholder() {
        let out = render_for_test(&[], None, 1024).unwrap();
        assert_eq!(out, "(empty result)");
    }

    #[test]
    fn render_is_error_becomes_err() {
        let err = render_for_test(&[part("boom")], Some(true), 1024).unwrap_err();
        assert!(err.contains("boom"), "{err:?}");
    }

    #[test]
    fn render_truncates_at_char_boundary() {
        let out = render_for_test(&[part("abcdef")], None, 3).unwrap();
        assert!(out.starts_with("abc"), "{out:?}");
        assert!(out.contains("truncated"), "{out:?}");
    }
}
