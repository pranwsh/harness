//! Rendering for `hashline_read`: `¶path#REV:{rev}` header plus
//! `{line}:{hash}|{content}` per line over the requested window.
//!
//! Line numbers are absolute (1-based into the file), so hashes from a
//! paginated read work directly with `hashline_edit`.

use std::sync::Arc;

use futures::future::BoxFuture;
use harness_contracts::ToolError;
use harness_hash_base::{HashStore, line_ranges, short_str};

use super::args::ReadArgs;

pub(crate) fn hashline_read_handler(
    store: Arc<HashStore>,
    args: String,
) -> BoxFuture<'static, harness_core::Result<String, ToolError>> {
    let parsed = super::args::parse_args(&args);
    match parsed {
        Ok(a) => Box::pin(async move {
            tokio::task::spawn_blocking(move || render(&store, &a))
                .await
                .map_err(|e| crate::tool_err(format!("read task failed: {e}")))?
        }),
        Err(e) => Box::pin(async move { Err(e) }),
    }
}

/// Render the requested window. Hashing always covers the whole file (see
/// [`HashStore::snapshot`]); `offset`/`limit` only trim the output.
pub(crate) fn render(
    store: &HashStore,
    a: &ReadArgs,
) -> std::result::Result<String, ToolError> {
    let (snap, bytes) = store
        .snapshot(&a.path)
        .map_err(|e| crate::tool_err(e.to_string()))?;
    let total = snap.entries.len();
    let paginated = a.offset != 1 || a.limit.is_some();

    // Header first so empty files (and empty windows) still report REV.
    let mut out = String::with_capacity(bytes.len() + snap.entries.len() * 8 + 64);
    out.push('¶');
    out.push_str(&snap.canonical.display().to_string());
    out.push_str("#REV:");
    out.push_str(&snap.rev.to_string());
    if total == 0 {
        if paginated {
            out.push_str(" lines 0 of 0");
        }
        return Ok(out);
    }
    if a.offset > total.max(1) as u64 {
        return Err(crate::tool_err(format!(
            "offset {} beyond EOF ({total} lines)",
            a.offset
        )));
    }
    let start = (a.offset - 1) as usize;
    let end = match a.limit {
        Some(l) => start.saturating_add(l as usize),
        None => total,
    }
    .min(total);
    if paginated {
        out.push_str(&format!(" lines {}-{end} of {total}", a.offset));
    }
    out.push('\n');

    let ranges = line_ranges(&bytes);
    for e in &snap.entries[start..end] {
        out.push_str(&e.lineno.to_string());
        out.push(':');
        out.push_str(short_str(&e.short));
        out.push('|');
        let (s, t) = ranges[(e.lineno - 1) as usize];
        let line = bytes[s..t].strip_suffix(b"\r").unwrap_or(&bytes[s..t]);
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_hash_base::HashStore;

    fn test_args(path: &std::path::Path, offset: u64, limit: Option<u64>) -> ReadArgs {
        ReadArgs {
            path: path.to_path_buf(),
            offset,
            limit,
        }
    }

    #[test]
    fn render_format_matches_spec() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("hashline-read-{}-fmt.txt", std::process::id()));
        std::fs::write(&path, "hello  \n  world\n").unwrap();
        let store = HashStore::new();
        let out = render(&store, &test_args(&path, 1, None)).unwrap();
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        assert!(header.starts_with('¶'), "header: {header}");
        assert!(header.contains("#REV:0"), "header: {header}");
        // Unpaginated header carries no window suffix.
        assert!(!header.contains("lines"), "header: {header}");
        let l1 = lines.next().unwrap();
        let l2 = lines.next().unwrap();
        assert!(l1.starts_with("1:"), "l1: {l1}");
        assert!(l1.ends_with("|hello  "), "l1: {l1}");
        assert!(l2.starts_with("2:"), "l2: {l2}");
        assert!(l2.ends_with("|  world"), "l2: {l2}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_file_returns_header_only() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-empty.txt", std::process::id()));
        std::fs::write(&path, "").unwrap();
        let store = HashStore::new();
        let out = render(&store, &test_args(&path, 1, None)).unwrap();
        assert_eq!(out.lines().count(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn offset_limit_window_uses_absolute_line_numbers() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-page.txt", std::process::id()));
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let store = HashStore::new();
        let out = render(&store, &test_args(&path, 2, Some(2))).unwrap();
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        assert!(header.contains("lines 2-3 of 5"), "header: {header}");
        let body: Vec<&str> = lines.collect();
        assert_eq!(body.len(), 2, "body: {body:?}");
        assert!(body[0].starts_with("2:"), "row: {}", body[0]);
        assert!(body[0].ends_with("|b"), "row: {}", body[0]);
        assert!(body[1].starts_with("3:"), "row: {}", body[1]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn limit_clamps_at_eof() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-clamp.txt", std::process::id()));
        std::fs::write(&path, "a\nb\n").unwrap();
        let store = HashStore::new();
        let out = render(&store, &test_args(&path, 2, Some(100))).unwrap();
        let header = out.lines().next().unwrap();
        assert!(header.contains("lines 2-2 of 2"), "header: {header}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn offset_beyond_eof_is_clean_error() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-oob.txt", std::process::id()));
        std::fs::write(&path, "a\n").unwrap();
        let store = HashStore::new();
        let err = render(&store, &test_args(&path, 9, None)).unwrap_err();
        assert!(err.message.contains("beyond EOF"), "got: {}", err.message);
        std::fs::remove_file(&path).ok();
    }
}
