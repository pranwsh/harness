//! Bounded output: tail truncation on UTF-8 boundaries + labeled format.
//!
//! Keeps the TAIL (errors/stack traces live at the end). Single pass,
//! pre-sized buffers, no regex.

/// Result of truncating one stream to its tail.
#[derive(Debug, Clone)]
pub struct Tail {
    pub text: String,
    pub truncated: bool,
    pub omitted_bytes: u64,
}

/// Keep the last `cap` bytes of `bytes`, starting on a UTF-8 boundary.
pub fn tail_truncate(bytes: &[u8], cap: usize) -> Tail {
    if bytes.len() <= cap {
        return Tail {
            text: String::from_utf8_lossy(bytes).into_owned(),
            truncated: false,
            omitted_bytes: 0,
        };
    }
    let mut start = bytes.len() - cap;
    // Advance past UTF-8 continuation bytes (max 3 steps).
    for _ in 0..4 {
        if start >= bytes.len() {
            break;
        }
        let b = bytes[start];
        if (b & 0xC0) != 0x80 {
            break;
        }
        start += 1;
    }
    Tail {
        text: String::from_utf8_lossy(&bytes[start..]).into_owned(),
        truncated: true,
        omitted_bytes: start as u64,
    }
}

/// Format a finished execution for the model.
pub fn format_result(
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: &[u8],
    stderr: &[u8],
    cap: usize,
) -> String {
    format_result_full(exit_code, timed_out, stdout, stderr, cap, 0, 0)
}

/// Format a finished shell execution for the model (Claude-style quiet
/// success).
///
/// A clean run (`exit 0`, no timeout, empty stderr, nothing truncated)
/// returns the raw stdout text with zero envelope overhead. Anything else
/// appends the streams that exist plus a one-line trailer carrying the
/// exit code and flags, so failures stay fully attributed without taxing
/// the common case.
pub fn format_shell_result(
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: &[u8],
    stderr: &[u8],
    cap: usize,
    ring_omitted_out: u64,
    ring_omitted_err: u64,
) -> String {
    let out = tail_truncate(stdout, cap);
    let err = tail_truncate(stderr, cap);
    let omitted_out = out.omitted_bytes + ring_omitted_out;
    let omitted_err = err.omitted_bytes + ring_omitted_err;
    let truncated = out.truncated || err.truncated || omitted_out > 0 || omitted_err > 0;

    if exit_code == Some(0) && !timed_out && err.text.is_empty() && !truncated {
        return out.text;
    }

    let mut s = String::with_capacity(out.text.len() + err.text.len() + 128);
    push_stream_sections(&mut s, &out.text, &err.text);
    s.push_str("[exit ");
    match exit_code {
        Some(c) => s.push_str(&c.to_string()),
        None => s.push_str("signal"),
    }
    s.push_str(&format!(", timed_out={timed_out}, truncated={truncated}"));
    if truncated {
        s.push_str(&format!(
            ", omitted_stdout={omitted_out}B, omitted_stderr={omitted_err}B"
        ));
    }
    s.push_str("]\n");
    s
}

/// Appends stream sections, omitting headers for empty streams: a lone
/// non-empty stream needs no label, while two non-empty streams get
/// `[stdout]`/`[stderr]` delineation. Shared by the sync formatter and
/// the background-job snapshot so the styles cannot drift apart.
pub fn push_stream_sections(s: &mut String, out_text: &str, err_text: &str) {
    if !out_text.is_empty() {
        if !err_text.is_empty() {
            s.push_str("[stdout]\n");
        }
        s.push_str(out_text);
        if !out_text.ends_with('\n') {
            s.push('\n');
        }
    }
    if !err_text.is_empty() {
        s.push_str("[stderr]\n");
        s.push_str(err_text);
        if !err_text.ends_with('\n') {
            s.push('\n');
        }
    }
}

/// Like [`format_result`] but adds upstream (ring-buffer) omission counts so
/// truncation reporting stays accurate when snapshots are already tails.
pub fn format_result_full(
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: &[u8],
    stderr: &[u8],
    cap: usize,
    ring_omitted_out: u64,
    ring_omitted_err: u64,
) -> String {
    let out = tail_truncate(stdout, cap);
    let err = tail_truncate(stderr, cap);
    let omitted_out = out.omitted_bytes + ring_omitted_out;
    let omitted_err = err.omitted_bytes + ring_omitted_err;
    let truncated = out.truncated || err.truncated || omitted_out > 0 || omitted_err > 0;
    let mut s = String::with_capacity(out.text.len() + err.text.len() + 128);
    s.push_str("[exit ");
    match exit_code {
        Some(c) => s.push_str(&c.to_string()),
        None => s.push_str("signal"),
    }
    s.push_str(&format!(", timed_out={timed_out}, truncated={truncated}"));
    if out.truncated || err.truncated || omitted_out > 0 || omitted_err > 0 {
        s.push_str(&format!(
            ", omitted_stdout={}B, omitted_stderr={}B",
            omitted_out, omitted_err
        ));
    }
    s.push_str("]\n[stdout]\n");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_not_truncated() {
        let t = tail_truncate(b"hello\n", 64);
        assert!(!t.truncated);
        assert_eq!(t.text, "hello\n");
        assert_eq!(t.omitted_bytes, 0);
    }

    #[test]
    fn tail_keeps_end() {
        let data: Vec<u8> = (0..1000).map(|i| b'0' + (i % 10) as u8).collect();
        let t = tail_truncate(&data, 100);
        assert!(t.truncated);
        assert_eq!(t.text.len(), 100);
        assert!(t.text.ends_with("9"));
        assert_eq!(t.omitted_bytes, 900);
    }

    #[test]
    fn truncation_respects_utf8_boundary() {
        // "é" = 2 bytes; cap landing mid-char must not panic.
        let mut data = vec![b'a'; 10];
        data.extend_from_slice("é".as_bytes());
        data.extend(vec![b'b'; 10]);
        let t = tail_truncate(&data, 11);
        assert!(std::str::from_utf8(t.text.as_bytes()).is_ok());
        assert!(t.text.ends_with('b'.to_string().repeat(10).as_str()));
    }

    #[test]
    fn format_labels_streams() {
        let s = format_result(Some(1), false, b"out\n", b"err\n", 64);
        assert!(s.contains("[exit 1"));
        assert!(s.contains("[stdout]"));
        assert!(s.contains("[stderr]"));
        assert!(s.contains("out"));
        assert!(s.contains("err"));
    }

    #[test]
    fn shell_result_quiet_success_returns_raw_stdout() {
        let s = format_shell_result(Some(0), false, b"hello\n", b"", 64, 0, 0);
        assert_eq!(s, "hello\n");
    }

    #[test]
    fn shell_result_nonzero_exit_carries_streams_and_trailer() {
        let s = format_shell_result(Some(3), false, b"out", b"err\n", 64, 0, 0);
        assert!(s.contains("[stdout]"), "got: {s}");
        assert!(s.contains("[stderr]"), "got: {s}");
        assert!(s.contains("out"));
        assert!(s.contains("err"));
        assert!(
            s.contains("[exit 3, timed_out=false, truncated=false]"),
            "got: {s}"
        );
    }

    #[test]
    fn shell_result_stdout_only_failure_skips_stderr_section() {
        let s = format_shell_result(Some(1), false, b"nope\n", b"", 64, 0, 0);
        assert!(!s.contains("[stderr]"), "got: {s}");
        assert!(!s.contains("[stdout]"), "got: {s}");
        assert!(s.contains("nope"));
        assert!(s.contains("[exit 1"), "got: {s}");
    }

    #[test]
    fn shell_result_stderr_only_failure_has_trailer() {
        let s = format_shell_result(Some(127), false, b"", b"not found\n", 64, 0, 0);
        assert!(s.contains("[stderr]"), "got: {s}");
        assert!(s.contains("[exit 127"), "got: {s}");
    }

    #[test]
    fn shell_result_timeout_reports_signal_or_timeout() {
        let s = format_shell_result(None, true, b"partial", b"", 64, 0, 0);
        assert!(s.contains("timed_out=true"), "got: {s}");
        let s = format_shell_result(Some(0), false, b"x", b"y", 64, 0, 0);
        assert!(s.contains("[stderr]"), "stderr forces envelope: {s}");
    }

    #[test]
    fn shell_result_truncation_is_flagged_with_counts() {
        let s = format_shell_result(Some(0), false, b"hello\n", b"", 4, 2, 0);
        assert!(s.contains("truncated=true"), "got: {s}");
        assert!(s.contains("omitted_stdout="), "got: {s}");
        assert!(s.contains("[exit 0"), "got: {s}");
    }
}
