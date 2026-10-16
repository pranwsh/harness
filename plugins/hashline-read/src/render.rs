//! Rendering for `hashline_read`.
//!
//! Default output is quiet — a `¶path#REV:{rev}` header (with a
//! `lines a-b of N` window suffix when the file is larger than the requested
//! window) followed by `{line}: {content}` rows. Pass `hashes: true` for the
//! edit-ready `{line}:{hash}|{content}` form used by `hashline_edit`.
//!
//! Line numbers are absolute (1-based into the file) in every mode, so hashes
//! from a paginated, hashed read work directly with `hashline_edit`.

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
/// [`HashStore::snapshot`]); `offset`/`limit`/`grep` only trim the output.
pub(crate) fn render(store: &HashStore, a: &ReadArgs) -> std::result::Result<String, ToolError> {
    // Directory guard: on Linux `File::open` succeeds for a directory and the
    // failure only surfaces later as an opaque `ENODEV` from `mmap`. Detect it
    // up front and say so plainly.
    if let Ok(meta) = std::fs::metadata(&a.path)
        && meta.is_dir()
    {
        return Err(crate::tool_err(format!(
            "{} is a directory, not a file; pass a file path (list it with `ls`)",
            a.path.display()
        )));
    }

    let (snap, bytes) = store
        .snapshot(&a.path)
        .map_err(|e| crate::tool_err(e.to_string()))?;
    let canonical = snap.canonical.display().to_string();
    let rev = snap.rev;

    // Cheap unchanged-check: if the caller's rev already matches, elide the body.
    if let Some(want) = a.if_rev_match
        && rev == want
    {
        return Ok(format!(
            "¶{canonical}#REV:{rev} UNCHANGED (rev {want} matches; body elided)"
        ));
    }

    let total = snap.entries.len();
    let ranges = line_ranges(&bytes);
    let line_text = |i: usize| -> std::borrow::Cow<'_, str> {
        let (s, t) = ranges[i];
        let raw = bytes[s..t].strip_suffix(b"\r").unwrap_or(&bytes[s..t]);
        String::from_utf8_lossy(raw)
    };

    // Candidate line indices after the optional substring filter.
    let selected: Vec<usize> = match &a.grep {
        Some(needle) => (0..total).filter(|&i| line_text(i).contains(needle.as_str())).collect(),
        None => (0..total).collect(),
    };
    let shown_total = selected.len();

    // Header first so empty files (and empty windows) still report REV.
    let mut out = String::with_capacity(bytes.len() + total * 8 + 64);
    out.push('¶');
    out.push_str(&canonical);
    out.push_str("#REV:");
    out.push_str(&rev.to_string());

    if total == 0 {
        return Ok(out);
    }

    let start = (a.offset - 1) as usize;
    if start >= shown_total {
        return Err(crate::tool_err(format!(
            "offset {} beyond EOF ({shown_total} lines{})",
            a.offset,
            if a.grep.is_some() {
                format!(" matching, of {total} total")
            } else {
                String::new()
            }
        )));
    }
    let end = match a.limit {
        Some(l) => start.saturating_add(l as usize),
        None => shown_total,
    }
    .min(shown_total);

    let windowed = a.offset != 1 || end < shown_total;
    if windowed {
        out.push_str(&format!(" lines {}-{end} of {shown_total}", a.offset));
        if end < shown_total {
            out.push_str(&format!(" (next offset {})", end + 1));
        }
    }
    if let Some(needle) = &a.grep {
        out.push_str(&format!(" grep={needle:?} ({shown_total} of {total} lines)"));
    }
    out.push('\n');

    for &i in &selected[start..end] {
        let e = &snap.entries[i];
        out.push_str(&e.lineno.to_string());
        if a.hashes {
            out.push(':');
            out.push_str(short_str(&e.short));
            out.push('|');
        } else {
            out.push_str(": ");
        }
        out.push_str(&line_text(i));
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
            hashes: true,
            if_rev_match: None,
            grep: None,
        }
    }

    fn quiet_args(path: &std::path::Path) -> ReadArgs {
        ReadArgs {
            path: path.to_path_buf(),
            offset: 1,
            limit: None,
            hashes: false,
            if_rev_match: None,
            grep: None,
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
    fn quiet_mode_omits_hashes() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-quiet.txt", std::process::id()));
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let store = HashStore::new();
        let out = render(&store, &quiet_args(&path)).unwrap();
        let mut lines = out.lines();
        let header = lines.next().unwrap();
        assert!(header.contains("#REV:0"), "header: {header}");
        assert_eq!(lines.next().unwrap(), "1: hello");
        assert_eq!(lines.next().unwrap(), "2: world");
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
        assert!(header.contains("next offset 4"), "header: {header}");
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
        assert!(!header.contains("next offset"), "header: {header}");
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

    #[test]
    fn directory_is_a_clean_error() {
        let store = HashStore::new();
        let dir = std::env::temp_dir();
        let err = render(&store, &test_args(&dir, 1, None)).unwrap_err();
        assert!(err.message.contains("is a directory"), "got: {}", err.message);
    }

    #[test]
    fn grep_filters_lines_and_reports_counts() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-grep.txt", std::process::id()));
        std::fs::write(&path, "alpha\nbeta\naxle\n").unwrap();
        let store = HashStore::new();
        let mut args = test_args(&path, 1, None);
        args.grep = Some("a".to_owned());
        let out = render(&store, &args).unwrap();
        let header = out.lines().next().unwrap();
        assert!(header.contains("grep="), "header: {header}");
        assert!(header.contains("3 of 3 lines"), "header: {header}");
        let body: Vec<&str> = out.lines().skip(1).collect();
        assert_eq!(body.len(), 3, "body: {body:?}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn if_rev_match_elides_body() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-rev.txt", std::process::id()));
        std::fs::write(&path, "a\nb\n").unwrap();
        let store = HashStore::new();
        // First read establishes REV:0.
        let _ = render(&store, &test_args(&path, 1, None)).unwrap();
        let mut args = test_args(&path, 1, None);
        args.if_rev_match = Some(0);
        let out = render(&store, &args).unwrap();
        assert!(out.contains("UNCHANGED"), "out: {out}");
        assert_eq!(out.lines().count(), 1, "out: {out}");
        std::fs::remove_file(&path).ok();
    }
}
