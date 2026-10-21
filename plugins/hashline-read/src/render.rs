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
use harness_hash_base::{HashStore, short_str};

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


/// Files larger than this are refused rather than hashed whole, because the
/// snapshot cost is O(file) even for a 50-line window.
pub(crate) const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// Render a directory as a listing instead of an error.
///
/// One tool call answers "what is here", which is what the caller wanted when
/// they pointed a file reader at a directory.
fn list_dir(
    path: &std::path::Path,
    needle: Option<&str>,
    ignore_case: bool,
) -> std::result::Result<String, ToolError> {
    let entries = std::fs::read_dir(path)
        .map_err(|e| crate::tool_err(format!("cannot list {}: {e}", path.display())))?;
    let mut names: Vec<(bool, String)> = Vec::new();
    for e in entries.flatten() {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let name = e.file_name().to_string_lossy().to_string();
        let keep = match needle {
            Some(n) if ignore_case => name.to_lowercase().contains(&n.to_lowercase()),
            Some(n) => name.contains(n),
            None => true,
        };
        if keep {
            // Trailing separator marks a directory, as `ls -p` does.
            names.push((is_dir, if is_dir { format!("{name}/") } else { name }));
        }
    }
    names.sort_by(|a, b| a.1.cmp(&b.1));
    let mut out = format!("¶{}/ (directory, {} entries)\n", path.display(), names.len());
    for (_, name) in names {
        out.push_str(&name);
        out.push('\n');
    }
    Ok(out)
}
/// Render the requested window. Hashing always covers the whole file (see
/// [`HashStore::snapshot`]); `offset`/`limit`/`grep` only trim the output.
pub(crate) fn render(store: &HashStore, a: &ReadArgs) -> std::result::Result<String, ToolError> {
    // A directory is not an error worth a round-trip to `ls`: list it. The
    // caller asked what is there, and one tool call answers that.
    if let Ok(meta) = std::fs::metadata(&a.path)
        && meta.is_dir()
    {
        return list_dir(&a.path, a.grep.as_deref(), a.ignore_case);
    }

    // Guard before hashing: `snapshot` hashes (and copies) the whole file
    // regardless of the window, so an accidental read of a multi-GB log would
    // cost a full scan and a full copy for 50 lines of output.
    if let Ok(meta) = std::fs::metadata(&a.path)
        && meta.len() > MAX_READ_BYTES
    {
        return Err(crate::tool_err(format!(
            "{} is {} bytes, over the {} byte read limit; \
             use `offset`/`limit` with `shell` (e.g. `sed -n '1,50p'`) or `grep` instead",
            a.path.display(),
            meta.len(),
            MAX_READ_BYTES
        )));
    }

    let (snap, bytes) = store
        .snapshot(&a.path)
        .map_err(|e| crate::tool_err(e.to_string()))?;
    // Refuse binary: splitting a PNG on `\n` and hashing the pieces yields
    // tags that address nothing a caller can meaningfully edit, and dumping
    // the bytes into the transcript is worse than the error.
    if harness_hash_base::looks_binary(&bytes) {
        return Err(crate::tool_err(format!(
            "{} looks like a binary file (NUL byte in the first 8 KiB); \
             hashline_read only reads text",
            a.path.display()
        )));
    }
    let canonical = snap.canonical.display().to_string();
    let rev = snap.rev;

    // Cheap unchanged-check: if the caller's rev already matches, elide the body.
    if let Some(want) = a.if_rev_match
        && rev == want
    {
        // Name what did not change, so the reply is self-contained.
        return Ok(format!(
            "¶{canonical}#REV:{rev} lines={} UNCHANGED (rev {want} matches; body elided)",
            snap.entries.len()
        ));
    }

    let total = snap.entries.len();
    // Ranges come from the snapshot: hashing already split the file, so
    // re-scanning here would be a second full pass for the same answer.
    let line_text = |i: usize| -> std::borrow::Cow<'_, str> {
        let (s, t) = snap.ranges[i];
        let raw = bytes[s..t].strip_suffix(b"\r").unwrap_or(&bytes[s..t]);
        String::from_utf8_lossy(raw)
    };

    // Candidate line indices after the optional substring filter.
    let needle = a.grep.as_deref();
    let matches = |text: &str| match needle {
        Some(n) if a.ignore_case => text.to_lowercase().contains(&n.to_lowercase()),
        Some(n) => text.contains(n),
        None => true,
    };
    let selected: Vec<usize> = (0..total).filter(|&i| matches(&line_text(i))).collect();
    let shown_total = selected.len();

    // Window is an index range into the selected rows; resolve it before
    // building the string so the buffer can be sized to the window.
    let start = (a.offset - 1) as usize;
    // Search terms can legitimately match nothing: that is an empty result,
    // not a bad offset. Only a real line offset out of range is an error.
    if start >= shown_total && needle.is_none() && total > 0 {
        return Err(crate::tool_err(format!(
            "offset {} beyond EOF ({total} lines)",
            a.offset
        )));
    }
    let start = start.min(shown_total);
    let end = match a.limit {
        Some(l) => start.saturating_add(l as usize),
        None => shown_total,
    }
    .min(shown_total);
    let shown = if total == 0 { &selected[0..0] } else { &selected[start..end] };

    // Header: `¶path#REV:rev lines=N` plus only the keys that apply, so a
    // caller can parse it without prose-suffix guessing.
    let mut out = String::with_capacity(64 + shown.len() * 80);
    out.push('¶');
    out.push_str(&canonical);
    out.push_str("#REV:");
    out.push_str(&rev.to_string());
    out.push_str(&format!(" lines={total}"));
    if needle.is_some() {
        out.push_str(&format!(" matches={shown_total}"));
    }
    if a.offset != 1 || end < shown_total {
        // Real file line numbers, not positions within the filtered list.
        let span = match (shown.first(), shown.last()) {
            (Some(f), Some(l)) => format!("{}-{}", snap.entries[*f].lineno, snap.entries[*l].lineno),
            _ => "none".to_owned(),
        };
        out.push_str(&format!(" window={span}"));
        if end < shown_total {
            out.push_str(&format!(" next={}", end + 1));
        }
    }
    out.push('\n');

    for &i in shown {
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
            ignore_case: false,
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
            ignore_case: false,
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
        // Unpaginated header reports the size but carries no window key.
        assert!(header.contains("lines=2"), "header: {header}");
        assert!(!header.contains("window="), "header: {header}");
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
        assert!(header.contains("lines=5"), "header: {header}");
        assert!(header.contains("window=2-3"), "header: {header}");
        assert!(header.contains("next=4"), "header: {header}");
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
        assert!(header.contains("lines=2"), "header: {header}");
        assert!(header.contains("window=2-2"), "header: {header}");
        assert!(!header.contains("next="), "header: {header}");
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

    /// Pointing a file reader at a directory answers the question instead of
    /// making the caller spend a round-trip on `ls`.
    #[test]
    fn directory_renders_a_listing() {
        let dir = std::env::temp_dir().join(format!("hashline-read-{}-dir", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let store = HashStore::new();
        let out = render(&store, &test_args(&dir, 1, None)).unwrap();
        assert!(out.contains("directory"), "out: {out}");
        assert!(out.contains("b.txt"), "out: {out}");
        assert!(out.contains("sub/"), "directories marked: {out}");
        // Listed in a stable order, with the file before the directory.
        let listing: Vec<&str> = out.lines().skip(1).collect();
        assert_eq!(listing, vec!["b.txt", "sub/"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_files_are_refused_before_hashing() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-big.txt", std::process::id()));
        // Sparse-ish: just enough to trip the limit without writing 32 MiB.
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_READ_BYTES + 1).unwrap();
        drop(f);
        let store = HashStore::new();
        let err = render(&store, &test_args(&path, 1, None)).unwrap_err();
        assert!(err.message.contains("over the"), "got: {}", err.message);
        std::fs::remove_file(&path).ok();
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
        assert!(header.contains("lines=3"), "header: {header}");
        assert!(header.contains("matches=3"), "header: {header}");
        let body: Vec<&str> = out.lines().skip(1).collect();
        assert_eq!(body.len(), 3, "body: {body:?}");
        std::fs::remove_file(&path).ok();
    }

    /// In grep mode the header's window must report real file line numbers,
    /// not positions within the filtered list — a caller edits by the former.
    #[test]
    fn grep_window_reports_real_line_numbers() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-grep-ln.txt", std::process::id()));
        std::fs::write(&path, "no\nhit one\nno\nhit two\n").unwrap();
        let store = HashStore::new();
        let mut args = test_args(&path, 1, Some(1));
        args.grep = Some("hit".to_owned());
        let out = render(&store, &args).unwrap();
        let header = out.lines().next().unwrap();
        assert!(header.contains("matches=2"), "header: {header}");
        // First match shown, and the window names its real line (2), not 1.
        assert!(header.contains("window=2-2"), "header: {header}");
        assert!(out.contains("|hit one"), "out: {out}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn grep_can_ignore_case() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-grep-i.txt", std::process::id()));
        std::fs::write(&path, "Alpha\nbeta\n").unwrap();
        let store = HashStore::new();
        let mut args = test_args(&path, 1, None);
        args.grep = Some("alpha".to_owned());
        // Case-sensitive by default: no match.
        assert_eq!(render(&store, &args).unwrap().lines().count(), 1);
        args.ignore_case = true;
        let out = render(&store, &args).unwrap();
        assert!(out.contains("Alpha"), "out: {out}");
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

    /// `if_rev_match` is a promise that the body did not change. An out-of-band
    /// write must break that promise by advancing the rev, not by silently
    /// eliding a body that no longer matches what the caller last saw.
    #[test]
    fn if_rev_match_does_not_elide_after_an_external_write() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-rev-ext.txt", std::process::id()));
        std::fs::write(&path, "a\nb\n").unwrap();
        let store = HashStore::new();
        let _ = render(&store, &test_args(&path, 1, None)).unwrap();

        // Somebody else rewrites the file.
        std::fs::write(&path, "a\nZZZ\n").unwrap();

        let mut args = test_args(&path, 1, None);
        args.if_rev_match = Some(0);
        let out = render(&store, &args).unwrap();
        assert!(!out.contains("UNCHANGED"), "stale rev must not elide: {out}");
        assert!(out.contains("ZZZ"), "the new body must be rendered: {out}");
        assert!(out.contains("ZZZ"), "the new body must be rendered: {out}");
        std::fs::remove_file(&path).ok();
    }

    /// A binary file has no lines a caller can edit meaningfully, and the
    /// bytes should not land in the transcript. Say so instead.
    #[test]
    fn binary_files_are_refused() {
        let path =
            std::env::temp_dir().join(format!("hashline-read-{}-bin.txt", std::process::id()));
        let original: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        std::fs::write(&path, original).unwrap();
        let store = HashStore::new();
        let err = render(&store, &test_args(&path, 1, None)).unwrap_err();
        assert!(err.message.contains("binary"), "got: {}", err.message);
        assert_eq!(std::fs::read(&path).unwrap(), original, "bytes stay intact");
        std::fs::remove_file(&path).ok();
    }
}
