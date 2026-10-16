//! Application of `hashline_edit` batches: validate-everything-first, then
//! a single atomic write.
//!
//! `rev` is an optional optimistic-concurrency check: when present it must
//! match, otherwise the in-transaction snapshot is authoritative. The whole
//! batch resolves against one fresh snapshot and applies bottom-up, so
//! multi-op batches never shift each other's anchors.
//! A missing file can only be materialized by a lone `create` op.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::future::BoxFuture;
use harness_contracts::ToolError;
use harness_hash_base::{FileSnapshot, HashError, HashStore, line_ranges, parse_short};

use super::ops::{Args, Op};

fn hash_to_tool(e: HashError) -> ToolError {
    crate::tool_err(e.to_string())
}

pub(crate) fn hashline_edit_handler(
    store: Arc<HashStore>,
    args: String,
) -> BoxFuture<'static, harness_core::Result<String, ToolError>> {
    let parsed: std::result::Result<Args, ToolError> = super::ops::parse_args(&args);
    match parsed {
        Ok(a) => Box::pin(async move {
            tokio::task::spawn_blocking(move || apply(&store, &a))
                .await
                .map_err(|e| crate::tool_err(format!("edit task failed: {e}")))?
        }),
        Err(e) => Box::pin(async move { Err(e) }),
    }
}

struct Resolved {
    /// Max affected original lineno (sort key, descending). Insert after
    /// HEAD -> 0.
    anchor: u32,
    kind: ResolvedKind,
}

enum ResolvedKind {
    Set {
        lineno: u32,
        content: Vec<u8>,
    },
    Insert {
        after: u32,
        lines: Vec<Vec<u8>>,
    },
    Delete {
        start: u32,
        end: u32,
    },
    Replace {
        start: u32,
        end: u32,
        lines: Vec<Vec<u8>>,
    },
}

fn resolve_one(snap: &FileSnapshot, hash: &str) -> std::result::Result<u32, HashError> {
    let short = parse_short(hash)?;
    let path = snap.canonical.display().to_string();
    let hits = snap.resolve(&short);
    match hits.as_slice() {
        [] => Err(HashError::UnknownHash {
            hash: hash.to_owned(),
            path,
            rev: snap.rev,
        }),
        [n] => Ok(*n),
        many => Err(HashError::Ambiguous {
            hash: hash.to_owned(),
            lines: many.to_vec(),
            path,
            rev: snap.rev,
        }),
    }
}

/// Bounds-check a 1-based line number against the snapshot.
fn check_lineno(snap: &FileSnapshot, n: u32, what: &str) -> std::result::Result<u32, ToolError> {
    let total = snap.entries.len() as u32;
    if n < 1 || n > total.max(1) {
        return Err(crate::tool_err(format!(
            "{what}={n} out of bounds ({total} lines)"
        )));
    }
    Ok(n)
}

/// Bounds-check an insert anchor (`0` = prepend, else 1-based line).
fn check_anchor(
    snap: &FileSnapshot,
    after: u32,
    what: &str,
) -> std::result::Result<u32, ToolError> {
    let total = snap.entries.len() as u32;
    if after > total {
        return Err(crate::tool_err(format!(
            "{what}={after} out of bounds ({total} lines)"
        )));
    }
    Ok(after)
}

/// Bounds-check an inclusive line range.
fn check_range(
    snap: &FileSnapshot,
    start: u32,
    end: u32,
    what: &str,
) -> std::result::Result<(u32, u32), ToolError> {
    let total = snap.entries.len() as u32;
    if start < 1 || end < 1 || start > end || end > total.max(1) {
        return Err(crate::tool_err(format!(
            "{what} range {start}-{end} invalid ({total} lines)"
        )));
    }
    Ok((start, end))
}

fn single_lines(lines: &[String], what: &str) -> std::result::Result<Vec<Vec<u8>>, ToolError> {
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        if l.contains('\n') || l.contains('\r') {
            return Err(crate::tool_err(format!(
                "{what} entries must be single lines without newline"
            )));
        }
        out.push(l.as_bytes().to_vec());
    }
    Ok(out)
}

pub(crate) fn apply(store: &HashStore, a: &Args) -> std::result::Result<String, ToolError> {
    // Serialize edits in-process for clean retry semantics; reads proceed
    // concurrently. Hold for the whole validate+write transaction.
    let _edit = store.lock_edit();
    let canonical = store.canonicalize(&PathBuf::from(&a.path));

    // Cross-process mutual exclusion (blocking) via per-file lockfile.
    let _fs = store
        .exclusive_file_lock(&canonical)
        .map_err(hash_to_tool)?;

    // Missing file: only a lone `create` op (rev 0) can materialize it.
    match std::fs::metadata(&canonical) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return create(store, &canonical, a);
        }
        Err(e) => return Err(hash_to_tool(HashError::Io(e))),
        Ok(_) => {}
    }
    if a.ops.iter().any(|op| matches!(op, Op::Create { .. })) {
        return Err(crate::tool_err(
            "file already exists: create is only for new files",
        ));
    }

    // 1. External-modification guard *before* re-hashing, plus the optional
    //    optimistic-concurrency check: `rev` supplied => must match.
    if let Some(want) = a.rev {
        store
            .check_rev(&canonical, want, "hashline_edit")
            .map_err(hash_to_tool)?;
    }
    let disk_meta = std::fs::metadata(&canonical).map_err(|e| {
        hash_to_tool(if e.kind() == std::io::ErrorKind::NotFound {
            HashError::NotFound(canonical.display().to_string())
        } else {
            HashError::Io(e)
        })
    })?;
    let disk_len = disk_meta.len();
    let disk_mtime = disk_meta.modified().unwrap_or(std::time::UNIX_EPOCH);
    store
        .check_external(&canonical, disk_mtime, disk_len, "hashline_edit")
        .map_err(hash_to_tool)?;

    // 2. Fresh snapshot (re-hashes; updates mtime/len, keeps rev).
    let (snap, bytes) = store.snapshot(&canonical).map_err(hash_to_tool)?;
    if let Some(want) = a.rev {
        debug_assert_eq!(snap.rev, want);
    }

    // 3. Resolve + validate every op before touching disk.
    let mut resolved: Vec<Resolved> = Vec::with_capacity(a.ops.len());
    for op in &a.ops {
        match op {
            Op::Set { hash, content } => {
                if content.contains('\n') || content.contains('\r') {
                    return Err(crate::tool_err(
                        "set.content must be a single line without newline",
                    ));
                }
                let n = resolve_one(&snap, hash).map_err(hash_to_tool)?;
                resolved.push(Resolved {
                    anchor: n,
                    kind: ResolvedKind::Set {
                        lineno: n,
                        content: content.as_bytes().to_vec(),
                    },
                });
            }
            Op::Insert { after_hash, lines } => {
                let ins = single_lines(lines, "insert.lines")?;
                let after = if after_hash == "HEAD" {
                    0
                } else {
                    resolve_one(&snap, after_hash).map_err(hash_to_tool)?
                };
                resolved.push(Resolved {
                    anchor: after,
                    kind: ResolvedKind::Insert { after, lines: ins },
                });
            }
            Op::Delete {
                start_hash,
                end_hash,
            } => {
                let s = resolve_one(&snap, start_hash).map_err(hash_to_tool)?;
                let e = resolve_one(&snap, end_hash).map_err(hash_to_tool)?;
                if s > e {
                    return Err(crate::tool_err(format!(
                        "delete range inverted: {start_hash}(line {s}) > {end_hash}(line {e})"
                    )));
                }
                resolved.push(Resolved {
                    anchor: e,
                    kind: ResolvedKind::Delete { start: s, end: e },
                });
            }
            Op::Replace {
                start_hash,
                end_hash,
                lines,
            } => {
                let ins = single_lines(lines, "replace.lines")?;
                let s = resolve_one(&snap, start_hash).map_err(hash_to_tool)?;
                let e = resolve_one(&snap, end_hash).map_err(hash_to_tool)?;
                if s > e {
                    return Err(crate::tool_err(format!(
                        "replace range inverted: {start_hash}(line {s}) > {end_hash}(line {e})"
                    )));
                }
                resolved.push(Resolved {
                    anchor: e,
                    kind: ResolvedKind::Replace {
                        start: s,
                        end: e,
                        lines: ins,
                    },
                });
            }
            Op::SetN { line, content } => {
                if content.contains('\n') || content.contains('\r') {
                    return Err(crate::tool_err(
                        "set_n.content must be a single line without newline",
                    ));
                }
                let n = check_lineno(&snap, *line, "set_n.line")?;
                resolved.push(Resolved {
                    anchor: n,
                    kind: ResolvedKind::Set {
                        lineno: n,
                        content: content.as_bytes().to_vec(),
                    },
                });
            }
            Op::InsertN { after, lines } => {
                let ins = single_lines(lines, "insert_n.lines")?;
                let a_ = check_anchor(&snap, *after, "insert_n.after")?;
                resolved.push(Resolved {
                    anchor: a_,
                    kind: ResolvedKind::Insert {
                        after: a_,
                        lines: ins,
                    },
                });
            }
            Op::DeleteN { start, end } => {
                let (st, en) = check_range(&snap, *start, *end, "delete_n")?;
                resolved.push(Resolved {
                    anchor: en,
                    kind: ResolvedKind::Delete { start: st, end: en },
                });
            }
            Op::ReplaceN { start, end, lines } => {
                let ins = single_lines(lines, "replace_n.lines")?;
                let (st, en) = check_range(&snap, *start, *end, "replace_n")?;
                resolved.push(Resolved {
                    anchor: en,
                    kind: ResolvedKind::Replace {
                        start: st,
                        end: en,
                        lines: ins,
                    },
                });
            }
            Op::Create { .. } => {
                // Unreachable: existing files reject `create` above.
                return Err(crate::tool_err(
                    "file already exists: create is only for new files",
                ));
            }
        }
    }

    // 4. Reject overlapping non-insert ranges (bottom-up is only well
    //    defined for disjoint edits; inserts at the same anchor are OK and
    //    keep input order).
    {
        let mut ranges: Vec<(u32, u32)> = resolved
            .iter()
            .filter_map(|r| match &r.kind {
                ResolvedKind::Set { lineno, .. } => Some((*lineno, *lineno)),
                ResolvedKind::Delete { start, end } => Some((*start, *end)),
                ResolvedKind::Replace { start, end, .. } => Some((*start, *end)),
                ResolvedKind::Insert { .. } => None,
            })
            .collect();
        ranges.sort();
        for w in ranges.windows(2) {
            if w[0].1 >= w[1].0 {
                return Err(crate::tool_err(
                    "overlapping ops in one batch; split into separate edits or re-read",
                ));
            }
        }
        // Insert anchored inside a deleted/replaced range is ambiguous.
        for r in &resolved {
            if let ResolvedKind::Insert { after, .. } = &r.kind {
                for (s, e) in &ranges {
                    if *after >= *s && *after < *e {
                        // `after == e` is fine (append after the range).
                        return Err(crate::tool_err(
                            "insert anchor lies inside a deleted range; re-read and retry",
                        ));
                    }
                }
            }
        }
    }

    // 5. Materialize lines (owned) and apply bottom-up.
    let had_trailing_nl = bytes.is_empty() || bytes.ends_with(b"\n");
    let mut lines: Vec<Vec<u8>> = line_ranges(&bytes)
        .iter()
        .map(|(s, e)| bytes[*s..*e].to_vec())
        .collect();
    // Strip CR from CRLF for round-trip stability.
    for l in &mut lines {
        if l.last() == Some(&b'\r') {
            l.pop();
        }
    }

    // Bottom-up: descending anchor; same-anchor inserts merged so input
    // order is preserved (sequential splice at one index would reverse).
    let mut with_idx: Vec<(usize, Resolved)> = resolved.into_iter().enumerate().collect();
    with_idx.sort_by(|(ai, a), (bi, b)| b.anchor.cmp(&a.anchor).then_with(|| ai.cmp(bi)));
    let mut ordered: Vec<Resolved> = Vec::with_capacity(with_idx.len());
    let mut drained = with_idx.into_iter().peekable();
    while let Some((_, cur)) = drained.next() {
        if !matches!(cur.kind, ResolvedKind::Insert { .. }) {
            ordered.push(cur);
            continue;
        }
        // Collect contiguous same-anchor inserts (already input-ordered).
        let anchor = cur.anchor;
        let mut buf_lines = match cur.kind {
            ResolvedKind::Insert { lines, .. } => lines,
            _ => unreachable!(),
        };
        while matches!(
            drained.peek(),
            Some((_, r)) if r.anchor == anchor && matches!(r.kind, ResolvedKind::Insert { .. })
        ) {
            let (_, nxt) = drained.next().expect("peeked");
            if let ResolvedKind::Insert { lines, .. } = nxt.kind {
                buf_lines.extend(lines);
            }
        }
        ordered.push(Resolved {
            anchor,
            kind: ResolvedKind::Insert {
                after: anchor,
                lines: buf_lines,
            },
        });
    }
    for r in ordered {
        match r.kind {
            ResolvedKind::Set { lineno, content } => {
                let i = (lineno - 1) as usize;
                if i >= lines.len() {
                    return Err(crate::tool_err(format!("set line {lineno} out of bounds")));
                }
                lines[i] = content;
            }
            ResolvedKind::Delete { start, end } => {
                let s = (start - 1) as usize;
                let e = end as usize; // inclusive -> exclusive
                if e > lines.len() || s >= e {
                    return Err(crate::tool_err(format!(
                        "delete range {start}-{end} out of bounds"
                    )));
                }
                lines.drain(s..e);
            }
            ResolvedKind::Replace {
                start,
                end,
                lines: ins,
            } => {
                let s = (start - 1) as usize;
                let e = end as usize; // inclusive -> exclusive
                if e > lines.len() || s >= e {
                    return Err(crate::tool_err(format!(
                        "replace range {start}-{end} out of bounds"
                    )));
                }
                lines.splice(s..e, ins);
            }
            ResolvedKind::Insert { after, lines: ins } => {
                let at = after as usize; // HEAD=0 -> prepend
                if at > lines.len() {
                    return Err(crate::tool_err(format!(
                        "insert anchor line {after} out of bounds"
                    )));
                }
                lines.splice(at..at, ins);
            }
        }
    }

    // 6. Serialize + atomic persist (temp in same dir + rename).
    let mut buf = Vec::with_capacity(bytes.len() + 1024);
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            buf.push(b'\n');
        }
        buf.extend_from_slice(l);
    }
    if !lines.is_empty() && had_trailing_nl {
        buf.push(b'\n');
    }

    let parent = canonical
        .parent()
        .ok_or_else(|| crate::tool_err("path has no parent"))?;
    store
        .atomic_write(
            &canonical,
            parent,
            &buf,
            Some(disk_meta.permissions()),
            "hashline_edit",
        )
        .map_err(hash_to_tool)?;
    store
        .commit_write(&canonical, "hashline_edit", a.ops.len())
        .map_err(hash_to_tool)
}

/// Materialize a missing file. Only a lone `create` op with `rev: 0` is
/// accepted; anything else is a caller error, not a creation request.
fn create(store: &HashStore, canonical: &Path, a: &Args) -> std::result::Result<String, ToolError> {
    let [Op::Create { lines }] = a.ops.as_slice() else {
        return Err(crate::tool_err(
            "file does not exist: create it with a single {\"op\":\"create\",\"lines\":[...]} op",
        ));
    };
    if matches!(a.rev, Some(n) if n != 0) {
        return Err(crate::tool_err("rev must be 0 when creating a new file"));
    }
    let ins = single_lines(lines, "create.lines")?;
    let parent = canonical
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| crate::tool_err("path has no parent"))?;
    if !parent.is_dir() {
        return Err(crate::tool_err(format!(
            "parent directory {} does not exist",
            parent.display()
        )));
    }
    let mut buf = Vec::with_capacity(ins.iter().map(|l| l.len() + 1).sum());
    for (i, l) in ins.iter().enumerate() {
        if i > 0 {
            buf.push(b'\n');
        }
        buf.extend_from_slice(l);
    }
    if !ins.is_empty() {
        buf.push(b'\n');
    }
    // Narrow the claim race before the atomic rename. Same best-effort
    // stance as the rest of cross-process handling (the lockfile only
    // coordinates harness processes).
    if canonical.exists() {
        return Err(crate::tool_err(
            "file already exists: create is only for new files",
        ));
    }
    store
        .atomic_write(canonical, parent, &buf, None, "hashline_edit")
        .map_err(hash_to_tool)?;
    store
        .commit_write(canonical, "hashline_edit", 1)
        .map_err(hash_to_tool)
}
#[cfg(test)]
mod tests {
    use super::*;
    use harness_hash_base::short_str;
    use std::path::Path;

    fn tmpfile(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hashline-edit-{}-{tag}.txt", std::process::id()))
    }

    fn read_hashes(store: &HashStore, path: &Path) -> Vec<String> {
        let (snap, _) = store.snapshot(path).unwrap();
        snap.entries
            .iter()
            .map(|e| short_str(&e.short).to_owned())
            .collect()
    }

    #[test]
    fn set_replaces_line() {
        let store = HashStore::new();
        let p = tmpfile("set");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![Op::Set {
                hash: h[1].clone(),
                content: "B".into(),
            }],
        };
        let out = apply(&store, &args).unwrap();
        assert!(out.contains("#REV:1"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nc\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn batch_applies_bottom_up() {
        let store = HashStore::new();
        let p = tmpfile("batch");
        std::fs::write(&p, "1\n2\n3\n4\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = Args {
            path: p.display().to_string(),
            rev: Some(0),
            // Deliberately ordered top-down; application must go bottom-up.
            ops: vec![
                Op::Set {
                    hash: h[0].clone(),
                    content: "one".into(),
                },
                Op::Delete {
                    start_hash: h[2].clone(),
                    end_hash: h[3].clone(),
                },
            ],
        };
        apply(&store, &args).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\n2\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn insert_head_and_after() {
        let store = HashStore::new();
        let p = tmpfile("ins");
        std::fs::write(&p, "b\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![Op::Insert {
                after_hash: "HEAD".into(),
                lines: vec!["a".into()],
            }],
        };
        apply(&store, &args).unwrap();
        // rev is now 1; re-read hashes for the second insert.
        let h2 = read_hashes(&store, &p);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n");
        let _ = (h, h2);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn stale_rev_fails_with_reread_guidance() {
        let store = HashStore::new();
        let p = tmpfile("stale");
        std::fs::write(&p, "a\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![Op::Set {
                hash: h[0].clone(),
                content: "b".into(),
            }],
        };
        apply(&store, &args).unwrap();
        let h2 = read_hashes(&store, &p);
        let retry = Args {
            path: p.display().to_string(),
            rev: Some(0), // stale
            ops: vec![Op::Set {
                hash: h2[0].clone(),
                content: "c".into(),
            }],
        };
        let err = apply(&store, &retry).unwrap_err();
        assert!(err.message.contains("stale"), "got: {}", err.message);
        assert!(
            err.message.contains("hashline_read"),
            "got: {}",
            err.message
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "b\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn failed_batch_leaves_file_untouched() {
        let store = HashStore::new();
        let p = tmpfile("atomic");
        std::fs::write(&p, "a\nb\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![
                Op::Set {
                    hash: h[0].clone(),
                    content: "A".into(),
                },
                Op::Set {
                    hash: "ZZZZ".into(),
                    content: "nope".into(),
                },
            ],
        };
        assert!(apply(&store, &args).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n");
        // No rev bump on failure.
        assert_eq!(store.current_rev(&store.canonicalize(&p)), Some(0));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn replace_spans_multiple_lines() {
        let store = HashStore::new();
        let p = tmpfile("replace");
        std::fs::write(&p, "a\nb\nc\nd\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![Op::Replace {
                start_hash: h[1].clone(),
                end_hash: h[2].clone(),
                lines: vec!["B1".into(), "B2".into(), "B3".into()],
            }],
        };
        let out = apply(&store, &args).unwrap();
        assert!(out.contains("#REV:1"), "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB1\nB2\nB3\nd\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn replace_rejects_inverted_range_and_overlap() {
        let store = HashStore::new();
        let p = tmpfile("replace-inv");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        let h = read_hashes(&store, &p);
        let inverted = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![Op::Replace {
                start_hash: h[2].clone(),
                end_hash: h[0].clone(),
                lines: vec!["x".into()],
            }],
        };
        let err = apply(&store, &inverted).unwrap_err();
        assert!(err.message.contains("inverted"), "got: {}", err.message);

        // Replace overlapping a set on the same line must fail as a batch.
        let overlapping = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![
                Op::Set {
                    hash: h[0].clone(),
                    content: "A".into(),
                },
                Op::Replace {
                    start_hash: h[0].clone(),
                    end_hash: h[1].clone(),
                    lines: vec!["x".into()],
                },
            ],
        };
        let err = apply(&store, &overlapping).unwrap_err();
        assert!(err.message.contains("overlapping"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\nc\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn create_materializes_missing_file_at_rev_1() {
        let store = HashStore::new();
        let p = tmpfile("create");
        std::fs::remove_file(&p).ok();
        let args = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![Op::Create {
                lines: vec!["a".into(), "b".into()],
            }],
        };
        let out = apply(&store, &args).unwrap();
        assert!(out.contains("#REV:1"), "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n");
        // Second create on the now-existing file fails; content untouched.
        let again = apply(&store, &args).unwrap_err();
        assert!(
            again.message.contains("already exists"),
            "got: {}",
            again.message
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn create_rejects_mixed_ops_nonzero_rev_and_missing_parent() {
        let store = HashStore::new();
        let p = tmpfile("create-bad");
        std::fs::remove_file(&p).ok();

        // Non-create ops on a missing file must say how to create it.
        let set = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![Op::Set {
                hash: "aaaa".into(),
                content: "x".into(),
            }],
        };
        let err = apply(&store, &set).unwrap_err();
        assert!(err.message.contains("create"), "got: {}", err.message);

        // Create mixed with other ops is rejected.
        let mixed = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![
                Op::Create {
                    lines: vec!["a".into()],
                },
                Op::Set {
                    hash: "aaaa".into(),
                    content: "x".into(),
                },
            ],
        };
        assert!(apply(&store, &mixed).is_err());
        assert!(!p.exists(), "mixed batch must not create the file");

        // Wrong rev is rejected before any write.
        let bad_rev = Args {
            path: p.display().to_string(),
            rev: Some(3),
            ops: vec![Op::Create {
                lines: vec!["a".into()],
            }],
        };
        let err = apply(&store, &bad_rev).unwrap_err();
        assert!(
            err.message.contains("rev must be 0"),
            "got: {}",
            err.message
        );
        assert!(!p.exists());

        // Missing parent directory is a clean error, not a staging failure.
        let orphan = PathBuf::from(format!(
            "{}/no-such-dir-{}/f.txt",
            std::env::temp_dir().display(),
            std::process::id()
        ));
        let orphan_args = Args {
            path: orphan.display().to_string(),
            rev: Some(0),
            ops: vec![Op::Create {
                lines: vec!["a".into()],
            }],
        };
        let err = apply(&store, &orphan_args).unwrap_err();
        assert!(
            err.message.contains("parent directory"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn line_number_ops_need_no_hashes_and_no_rev() {
        let store = HashStore::new();
        let p = tmpfile("lineno");
        std::fs::write(&p, "a\nb\nc\nd\n").unwrap();
        let args = Args {
            path: p.display().to_string(),
            rev: None, // no hashline_read needed at all
            ops: vec![
                Op::SetN {
                    line: 2,
                    content: "B".into(),
                },
                Op::DeleteN { start: 4, end: 4 },
            ],
        };
        let out = apply(&store, &args).unwrap();
        assert!(out.contains("#REV:1"), "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nc\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn line_number_replace_and_insert_n() {
        let store = HashStore::new();
        let p = tmpfile("lineno2");
        std::fs::write(&p, "1\n2\n3\n").unwrap();
        let args = Args {
            path: p.display().to_string(),
            rev: None,
            ops: vec![
                Op::ReplaceN {
                    start: 1,
                    end: 2,
                    lines: vec!["one".into()],
                },
                Op::InsertN {
                    after: 0,
                    lines: vec!["top".into()],
                },
            ],
        };
        apply(&store, &args).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "top\none\n3\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn line_number_out_of_bounds_is_clean_error() {
        let store = HashStore::new();
        let p = tmpfile("lineno-bad");
        std::fs::write(&p, "a\nb\n").unwrap();
        for ops in [
            vec![Op::SetN {
                line: 9,
                content: "x".into(),
            }],
            vec![Op::DeleteN { start: 2, end: 1 }],
            vec![Op::InsertN {
                after: 7,
                lines: vec!["x".into()],
            }],
        ] {
            let args = Args {
                path: p.display().to_string(),
                rev: None,
                ops,
            };
            let err = apply(&store, &args).unwrap_err();
            assert!(
                err.message.contains("out of bounds") || err.message.contains("invalid"),
                "got: {}",
                err.message
            );
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn supplied_rev_still_guards_line_number_ops() {
        let store = HashStore::new();
        let p = tmpfile("lineno-rev");
        std::fs::write(&p, "a\n").unwrap();
        let ok = Args {
            path: p.display().to_string(),
            rev: None,
            ops: vec![Op::SetN {
                line: 1,
                content: "b".into(),
            }],
        };
        apply(&store, &ok).unwrap();
        // rev is now 1; Some(0) must fail.
        let stale = Args {
            path: p.display().to_string(),
            rev: Some(0),
            ops: vec![Op::SetN {
                line: 1,
                content: "c".into(),
            }],
        };
        let err = apply(&store, &stale).unwrap_err();
        assert!(err.message.contains("stale"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "b\n");
        std::fs::remove_file(&p).ok();
    }
}
