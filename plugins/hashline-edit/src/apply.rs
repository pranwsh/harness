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

/// Resolve a hash tag to exactly one line.
///
/// `line` is an optional composite anchor: with it, a tag shared by several
/// identical lines resolves to the named line (if the tag is there at all),
/// so duplicates stop being an `ambiguous` dead end. It also makes a stale
/// tag a loud mismatch instead of a silent wrong-line edit.
fn resolve_one(
    snap: &FileSnapshot,
    hash: &str,
    line: Option<u32>,
) -> std::result::Result<u32, HashError> {
    let short = parse_short(hash)?;
    let path = snap.canonical.display().to_string();
    let hits = snap.resolve(&short);
    if let Some(want) = line {
        return if hits.contains(&want) {
            Ok(want)
        } else if hits.is_empty() {
            Err(HashError::UnknownHash {
                hash: hash.to_owned(),
                path,
                rev: snap.rev,
            })
        } else {
            // The tag exists, just not on the line the caller named: the
            // read is stale relative to this file.
            Err(HashError::TagLineMismatch {
                hash: hash.to_owned(),
                line: want,
                lines: hits,
                path,
                rev: snap.rev,
            })
        };
    }
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
    // An empty file has no line 1: say that instead of letting `n` pass here
    // and failing later with a different message from the splice.
    if total == 0 {
        return Err(crate::tool_err(format!("file is empty; {what} has no line to address")));
    }
    if n < 1 || n > total {
        return Err(crate::tool_err(format!("{what}={n} out of bounds ({total} lines)")));
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
    if total == 0 {
        return Err(crate::tool_err(format!("file is empty; {what} has no lines to range over")));
    }
    if start < 1 || end < 1 || start > end || end > total {
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

/// Compact line diff of a would-be edit: `(-)`/`(+)` on changed spans only.
///
/// Returns the rendered hunks plus added/removed counts. Unchanged runs are
/// shown as their line count, so a one-line change in a 2000-line file is a
/// three-line reply rather than a 2000-line one.
fn summarize_diff(before: &[String], after: &[String]) -> (String, usize, usize) {
    // Trim the common head and tail, then diff the middle by simple LCS-free
    // pairing: this is for human confirmation, not patch application.
    let mut head = 0;
    while head < before.len() && head < after.len() && before[head] == after[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < before.len() - head
        && tail < after.len() - head
        && before[before.len() - 1 - tail] == after[after.len() - 1 - tail]
    {
        tail += 1;
    }
    let removed_span = &before[head..before.len() - tail];
    let added_span = &after[head..after.len() - tail];

    let mut out = String::new();
    if head > 0 {
        out.push_str(&format!("  {head} unchanged\n"));
    }
    for l in removed_span {
        out.push('-');
        out.push_str(l);
        out.push('\n');
    }
    for l in added_span {
        out.push('+');
        out.push_str(l);
        out.push('\n');
    }
    if tail > 0 {
        out.push_str(&format!("  {tail} unchanged\n"));
    }
    (out, added_span.len(), removed_span.len())
}

/// The line ending a file already uses, so an edit does not silently rewrite
/// every line in the file.
///
/// Mixed endings are reported rather than guessed at: normalizing them would
/// be a whole-file diff disguised as a one-line change, and any choice we
/// made would be wrong for half the file.
fn detect_eol(bytes: &[u8]) -> std::result::Result<&'static str, ToolError> {
    let crlf = bytes.windows(2).filter(|w| w == b"\r\n").count();
    let lf = bytes
        .iter()
        .enumerate()
        .filter(|(i, b)| **b == b'\n' && (*i == 0 || bytes[i - 1] != b'\r'))
        .count();
    match (crlf, lf) {
        (0, _) => Ok("\n"),
        (_, 0) => Ok("\r\n"),
        _ => Err(crate::tool_err(format!(
            "file mixes CRLF ({crlf} lines) and LF ({lf} lines) endings; \
             normalize it first (e.g. with `sed`) so an edit cannot rewrite them all"
        ))),
    }
}

/// Refuse to line-edit content that is not text.
///
/// Splitting a binary file on `\\n` and writing it back is data loss, and the
/// caller almost certainly meant a different path.
fn guard_text(bytes: &[u8], path: &Path) -> std::result::Result<(), ToolError> {
    if harness_hash_base::looks_binary(bytes) {
        return Err(crate::tool_err(format!(
            "{} looks like a binary file (NUL byte in the first 8 KiB); \
             hashline_edit only edits text",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn apply(store: &HashStore, a: &Args) -> std::result::Result<String, ToolError> {
    let canonical = store.canonicalize(&PathBuf::from(&a.path));
    // Per-file exclusion (threads and processes) held for the whole
    // validate+write transaction. Reads never take it, and edits to other
    // files proceed in parallel.
    let _edit = store.lock_edit(&canonical).map_err(hash_to_tool)?;

    // Missing file: only a lone `create` op (rev 0) can materialize it.
    match std::fs::metadata(&canonical) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return create(store, &canonical, a);
        }
        Err(e) => return Err(hash_to_tool(HashError::Io(e))),
        Ok(_) => {}
    }
    // `revert` restores the whole file, so it must stand alone.
    if a.ops.iter().any(|op| matches!(op, Op::Revert)) {
        let [Op::Revert] = a.ops.as_slice() else {
            return Err(crate::tool_err(
                "revert must be the only op in the batch; it replaces the whole file",
            ));
        };
        return revert(store, &canonical, a);
    }
    if a.ops.iter().any(|op| matches!(op, Op::Create { .. })) {
        return Err(crate::tool_err(
            "file already exists: create is only for new files",
        ));
    }

    // 1. External-modification guard *before* re-hashing: did the bytes move
    //    under us since we last looked?
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

    // 2. Remember the rev a caller's line numbers were most likely read at.
    //    Reading it *before* the snapshot matters: snapshotting marks a fresh
    //    caller-visible view, which would erase the very gap we want to catch.
    let read_rev = store.read_rev(&canonical);
    //    Fresh snapshot. This is the authoritative revision: it re-hashes the
    //    bytes we are about to edit, so both the `rev` check and the
    //    line-number guard compare against what we actually see.
    let (snap, bytes) = store.snapshot(&canonical).map_err(hash_to_tool)?;
    guard_text(&bytes, &canonical)?;
    if let Some(want) = a.rev {
        store
            .check_rev(&canonical, want, "hashline_edit")
            .map_err(hash_to_tool)?;
        debug_assert_eq!(snap.rev, want);
    }

    // 3. Line-number ops are addressed against whatever the caller last read.
    //    If we have edited since then, those numbers may point at the wrong
    //    lines and nothing else would catch it (our own writes keep the
    //    external-modification guard quiet). Fail loudly instead of
    //    silently editing the wrong line.
    //    `sub` counts when it anchors by number; by hash it is safe like any
    //    other hash op.
    let uses_line_numbers = a.ops.iter().any(|op| {
        matches!(
            op,
            Op::SetN { .. }
                | Op::InsertN { .. }
                | Op::DeleteN { .. }
                | Op::ReplaceN { .. }
                | Op::Sub {
                    hash: None,
                    ..
                }
        )
    });
    if uses_line_numbers
        && let Some(seen) = read_rev
        && seen != snap.rev
    {
        return Err(crate::tool_err(format!(
            "line-number ops are addressed against a read at REV:{seen}, \
             but {path} is now REV:{rev}; re-read the file and retry with fresh numbers, \
             or address the lines by hash instead",
            path = canonical.display(),
            rev = snap.rev,
        )));
    }

    // 4. Resolve + validate every op before touching disk.
    let mut resolved: Vec<Resolved> = Vec::with_capacity(a.ops.len());
    for op in &a.ops {
        match op {
            Op::Set {
                hash,
                line,
                content,
            } => {
                if content.contains('\n') || content.contains('\r') {
                    return Err(crate::tool_err(
                        "set.content must be a single line without newline",
                    ));
                }
                let n = resolve_one(&snap, hash, *line).map_err(hash_to_tool)?;
                resolved.push(Resolved {
                    anchor: n,
                    kind: ResolvedKind::Set {
                        lineno: n,
                        content: content.as_bytes().to_vec(),
                    },
                });
            }
            Op::Insert {
                after_hash,
                line,
                lines,
            } => {
                let ins = single_lines(lines, "insert.lines")?;
                let after = if after_hash == "HEAD" {
                    0
                } else {
                    resolve_one(&snap, after_hash, *line).map_err(hash_to_tool)?
                };
                resolved.push(Resolved {
                    anchor: after,
                    kind: ResolvedKind::Insert { after, lines: ins },
                });
            }
            Op::Delete {
                start_hash,
                start_line,
                end_hash,
                end_line,
            } => {
                let s = resolve_one(&snap, start_hash, *start_line).map_err(hash_to_tool)?;
                let e = resolve_one(&snap, end_hash, *end_line).map_err(hash_to_tool)?;
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
                start_line,
                end_hash,
                end_line,
                lines,
            } => {
                let ins = single_lines(lines, "replace.lines")?;
                let s = resolve_one(&snap, start_hash, *start_line).map_err(hash_to_tool)?;
                let e = resolve_one(&snap, end_hash, *end_line).map_err(hash_to_tool)?;
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
            Op::Sub {
                line,
                hash,
                old,
                new,
                count,
            } => {
                // Anchor by number or by hash — whichever the caller has.
                let n = match (line, hash) {
                    (Some(l), _) => check_lineno(&snap, *l, "sub.line")?,
                    (None, Some(h)) => resolve_one(&snap, h, None).map_err(hash_to_tool)?,
                    (None, None) => {
                        return Err(crate::tool_err(
                            "sub needs a `line` or a `hash` to say which line to change",
                        ));
                    }
                };
                let want = count.unwrap_or(1).max(1);
                let idx = (n - 1) as usize;
                let (s, e) = line_ranges(&bytes)[idx];
                let text = String::from_utf8_lossy(&bytes[s..e]).to_string();
                let found = text.matches(old.as_str()).count();
                if found != want as usize {
                    return Err(crate::tool_err(format!(
                        "sub.line {n}: expected {want} occurrence(s) of {old:?} but found {found}; \
                         nothing was changed"
                    )));
                }
                let updated = text.replacen(old.as_str(), new, want as usize);
                if updated.contains('\n') || updated.contains('\r') {
                    return Err(crate::tool_err(
                        "sub.new must not introduce a line break; use replace lines instead",
                    ));
                }
                resolved.push(Resolved {
                    anchor: n,
                    kind: ResolvedKind::Set {
                        lineno: n,
                        content: updated.into_bytes(),
                    },
                });
            }
            Op::Create { .. } => {
                // Unreachable: existing files reject `create` above.
                return Err(crate::tool_err(
                    "file already exists: create is only for new files",
                ));
            }
            Op::Revert => {
                // Unreachable: `revert` returns before op resolution.
                return Err(crate::tool_err(
                    "revert must be the only op in the batch; it replaces the whole file",
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
                let (a, b) = (&w[0], &w[1]);
                return Err(crate::tool_err(format!("overlapping ops in one batch (lines {}-{} and {}-{}); split into separate edits or re-read", a.0, a.1, b.0, b.1)));
            }
        }
        // Insert anchored inside a deleted/replaced range is ambiguous.
        for r in &resolved {
            if let ResolvedKind::Insert { after, .. } = &r.kind {
                for (s, e) in &ranges {
                    if *after >= *s && *after < *e {
                        // `after == e` is fine (append after the range).
                        return Err(crate::tool_err(format!("insert anchor line {after} lies inside the deleted/replaced range {s}-{e}; re-read and retry")));
                    }
                }
            }
        }
    }

    // 5. Materialize lines (owned) and apply bottom-up.
    let had_trailing_nl = bytes.is_empty() || bytes.ends_with(b"\n");
    // How this file ends its lines. Rewriting every line ending because one
    // line changed is exactly the kind of invisible churn that makes a
    // one-line edit look like a whole-file diff, so round-trip it instead.
    let eol = detect_eol(&bytes)?;
    let mut lines: Vec<Vec<u8>> = line_ranges(&bytes)
        .iter()
        .map(|(s, e)| bytes[*s..*e].to_vec())
        .collect();
    // Strip CR from CRLF so hashing, matching, and splicing all speak in
    // bare lines; `eol` puts the ending back on the way out.
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

    // 6. Serialize + atomic persist (temp in same dir + rename), re-applying
    //    the file's own line ending.
    let mut buf = Vec::with_capacity(bytes.len() + 1024);
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            buf.extend_from_slice(eol.as_bytes());
        }
        buf.extend_from_slice(l);
    }
    if !lines.is_empty() && had_trailing_nl {
        buf.extend_from_slice(eol.as_bytes());
    }

    // What the caller would see change. Computed for every batch because it
    // is also the dry-run payload; cheap relative to the write itself.
    let before: Vec<String> = line_ranges(&bytes)
        .iter()
        .map(|(s, e)| {
            String::from_utf8_lossy(bytes[*s..*e].strip_suffix(b"\r").unwrap_or(&bytes[*s..*e]))
                .to_string()
        })
        .collect();
    let after: Vec<String> = lines
        .iter()
        .map(|l| String::from_utf8_lossy(l).to_string())
        .collect();
    let (diff, added, removed) = summarize_diff(&before, &after);

    // Dry run stops here: everything above validated exactly as a real batch
    // would, so the diff shown is the edit that would have landed.
    if a.dry_run {
        return Ok(format!(
            "DRY RUN {}#REV:{rev} ops={ops} (+{added}/-{removed} lines)\n{diff}",
            canonical.display(),
            rev = snap.rev,
            ops = a.ops.len(),
            added = added,
            removed = removed,
            diff = diff,
        ));
    }

    let parent = canonical
        .parent()
        .ok_or_else(|| crate::tool_err("path has no parent"))?;
    // Fold exactly the bytes we are writing, so the rev table agrees with the
    // next read instead of needing a re-hash to notice a change.
    let digest = store.fold_bytes(&buf);
    // Keep what we are replacing so the caller can undo one step.
    store.stash_undo(&canonical, &bytes, snap.rev);
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
        .commit_write(&canonical, digest, buf.len() as u64, a.ops.len())
        .map_err(hash_to_tool)
}

/// Materialize a missing file. Only a lone `create` op with `rev: 0` is
/// accepted; anything else is a caller error, not a creation request.
/// Restore the file to the bytes the last edit replaced.
///
/// Consumes the undo slot: a second `revert` reports that there is nothing to
/// restore rather than replaying an older state. The restore itself goes
/// through the same atomic write, and the caller's `rev` is still checked so
/// an interleaved external edit is not silently clobbered.
fn revert(
    store: &HashStore,
    canonical: &Path,
    a: &Args,
) -> std::result::Result<String, ToolError> {
    if let Some(want) = a.rev {
        store
            .check_rev(canonical, want, "hashline_edit")
            .map_err(hash_to_tool)?;
    }
    let Some((bytes, _old_rev)) = store.take_undo(canonical) else {
        return Err(crate::tool_err(format!(
            "nothing to revert for {}: no edit recorded in this session (or it was already undone)",
            canonical.display()
        )));
    };
    let parent = canonical
        .parent()
        .ok_or_else(|| crate::tool_err("path has no parent"))?;
    let perm = std::fs::metadata(canonical).ok().map(|m| m.permissions());
    let digest = store.fold_bytes(&bytes);
    store
        .atomic_write(canonical, parent, &bytes, perm, "hashline_edit")
        .map_err(hash_to_tool)?;
    let out = store
        .commit_write(canonical, digest, bytes.len() as u64, 1)
        .map_err(hash_to_tool)?;
    Ok(out.replacen("ops=1", "ops=1 reverted", 1))
}
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
    let digest = store.fold_bytes(&buf);
    store
        .atomic_write(canonical, parent, &buf, None, "hashline_edit")
        .map_err(hash_to_tool)?;
    store
        .commit_write(canonical, digest, buf.len() as u64, 1)
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
            dry_run: false,
            ops: vec![Op::Set {
                hash: h[1].clone(),
                line: None,
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
            dry_run: false,
            // Deliberately ordered top-down; application must go bottom-up.
            ops: vec![
                Op::Set {
                    hash: h[0].clone(),
                    line: None,
                    content: "one".into(),
                },
                Op::Delete {
                    start_hash: h[2].clone(),
                    start_line: None,
                    end_hash: h[3].clone(),
                end_line: None,
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
            dry_run: false,
            ops: vec![Op::Insert {
                after_hash: "HEAD".into(),
                line: None,
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
            dry_run: false,
            ops: vec![Op::Set {
                hash: h[0].clone(),
                line: None,
                content: "b".into(),
            }],
        };
        apply(&store, &args).unwrap();
        let h2 = read_hashes(&store, &p);
        let retry = Args {
            path: p.display().to_string(),
            rev: Some(0), // stale
            dry_run: false,
            ops: vec![Op::Set {
                hash: h2[0].clone(),
                line: None,
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
            dry_run: false,
            ops: vec![
                Op::Set {
                    hash: h[0].clone(),
                    line: None,
                    content: "A".into(),
                },
                Op::Set {
                    hash: "ZZZZ".into(),
                    line: None,
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
            dry_run: false,
            ops: vec![Op::Replace {
                start_hash: h[1].clone(),
                start_line: None,
                end_hash: h[2].clone(),
                end_line: None,
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
            dry_run: false,
            ops: vec![Op::Replace {
                start_hash: h[2].clone(),
                start_line: None,
                end_hash: h[0].clone(),
                end_line: None,
                lines: vec!["x".into()],
            }],
        };
        let err = apply(&store, &inverted).unwrap_err();
        assert!(err.message.contains("inverted"), "got: {}", err.message);

        // Replace overlapping a set on the same line must fail as a batch.
        let overlapping = Args {
            path: p.display().to_string(),
            rev: Some(0),
            dry_run: false,
            ops: vec![
                Op::Set {
                    hash: h[0].clone(),
                    line: None,
                    content: "A".into(),
                },
                Op::Replace {
                    start_hash: h[0].clone(),
                    start_line: None,
                    end_hash: h[1].clone(),
                    end_line: None,
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
            dry_run: false,
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
            dry_run: false,
            ops: vec![Op::Set {
                hash: "aaaa".into(),
                line: None,
                content: "x".into(),
            }],
        };
        let err = apply(&store, &set).unwrap_err();
        assert!(err.message.contains("create"), "got: {}", err.message);

        // Create mixed with other ops is rejected.
        let mixed = Args {
            path: p.display().to_string(),
            rev: Some(0),
            dry_run: false,
            ops: vec![
                Op::Create {
                    lines: vec!["a".into()],
                },
                Op::Set {
                    hash: "aaaa".into(),
                    line: None,
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
            dry_run: false,
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
            dry_run: false,
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
            dry_run: false,
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
            dry_run: false,
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
                dry_run: false,
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
            dry_run: false,
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
            dry_run: false,
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

    /// The silent-mis-edit case: read, edit, then reuse the *old* line
    /// numbers with no `rev`. The bytes moved under those numbers, so this
    /// must fail loudly rather than edit line 1 of a file it never saw.
    #[test]
    fn line_number_ops_after_a_prior_edit_are_refused() {
        let store = HashStore::new();
        let p = tmpfile("lineno-reread");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        // Read establishes REV:0 and makes the caller's numbers "current".
        store.snapshot(&p).unwrap();

        // First edit lands fine (addressed against REV:0).
        apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::InsertN {
                    after: 0,
                    lines: vec!["top".into()],
                }],
            },
        )
        .unwrap();

        // Second edit reuses the pre-insert numbering (line 2 == "b"), which
        // now points at "a". Without the guard this would silently corrupt.
        let stale_numbers = Args {
            path: p.display().to_string(),
            rev: None,
            dry_run: false,
            ops: vec![Op::SetN {
                line: 2,
                content: "B".into(),
            }],
        };
        let err = apply(&store, &stale_numbers).unwrap_err();
        assert!(err.message.contains("REV:0"), "got: {}", err.message);
        assert!(
            err.message.contains("re-read"),
            "the error must say how to recover: {}",
            err.message
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "top\na\nb\nc\n",
            "the refused batch must not touch the file"
        );

        // Re-reading re-synchronizes the numbers and the edit goes through.
        store.snapshot(&p).unwrap();
        apply(&store, &stale_numbers).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "top\nB\nb\nc\n");
        std::fs::remove_file(&p).ok();
    }

    /// Two edits back to back: the second must not trip the
    /// external-modification guard on our own first write.
    #[test]
    fn consecutive_edits_do_not_look_external() {
        let store = HashStore::new();
        let p = tmpfile("consecutive");
        std::fs::write(&p, "a\n").unwrap();
        store.snapshot(&p).unwrap();
        for (i, want) in [(1u32, "b\n"), (2, "c\n")] {
            apply(
                &store,
                &Args {
                    path: p.display().to_string(),
                    rev: None,
                    dry_run: false,
                    ops: vec![Op::ReplaceN {
                        start: 1,
                        end: 1,
                        lines: vec![want.trim().to_owned()],
                    }],
                },
            )
            .unwrap_or_else(|e| panic!("edit {i} failed: {}", e.message));
            assert_eq!(std::fs::read_to_string(&p).unwrap(), want);
            // Each successful write is visible to a read before the next edit.
            assert_eq!(store.snapshot(&p).unwrap().0.rev, i as u64);
        }
        std::fs::remove_file(&p).ok();
    }

    /// A one-line edit in a CRLF file must stay a one-line diff.
    #[test]
    fn crlf_line_endings_round_trip() {
        let store = HashStore::new();
        let p = tmpfile("crlf");
        std::fs::write(&p, "a\r\nb\r\nc\r\n").unwrap();
        apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::SetN {
                    line: 2,
                    content: "B".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\r\nB\r\nc\r\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn lf_line_endings_are_not_converted_to_crlf() {
        let store = HashStore::new();
        let p = tmpfile("lf");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::SetN {
                    line: 2,
                    content: "B".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nc\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn mixed_line_endings_are_refused_rather_than_normalized() {
        let store = HashStore::new();
        let p = tmpfile("mixed-eol");
        std::fs::write(&p, "a\r\nb\nc\n").unwrap();
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::SetN {
                    line: 1,
                    content: "A".into(),
                }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("mixes CRLF"), "got: {}", err.message);
        // Untouched.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\r\nb\nc\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn binary_content_is_refused() {
        let store = HashStore::new();
        let p = tmpfile("binary");
        let original: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        std::fs::write(&p, original).unwrap();
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::SetN {
                    line: 1,
                    content: "x".into(),
                }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("binary"), "got: {}", err.message);
        assert_eq!(std::fs::read(&p).unwrap(), original, "bytes must be intact");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sub_replaces_a_substring_in_place() {
        let store = HashStore::new();
        let p = tmpfile("sub");
        let long = format!("let {} = call({}, {});", "x", "1", "2");
        std::fs::write(&p, format!("{long}\n")).unwrap();
        apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Sub {
                    line: Some(1),
                    hash: None,
                    old: "1".into(),
                    new: "42".into(),
                    count: None,
                }],
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            format!("let x = call(42, 2);\n")
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sub_can_anchor_by_hash_and_honours_count() {
        let store = HashStore::new();
        let p = tmpfile("sub-hash");
        std::fs::write(&p, "a a a\nb\n").unwrap();
        let h = read_hashes(&store, &p);
        // Wrong count is refused and nothing is written.
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Sub {
                    line: None,
                    hash: Some(h[0].clone()),
                    old: "a".into(),
                    new: "z".into(),
                    count: Some(2),
                }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("found 3"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a a a\nb\n");

        // Correct count goes through.
        apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Sub {
                    line: None,
                    hash: Some(h[0].clone()),
                    old: "a".into(),
                    new: "z".into(),
                    count: Some(3),
                }],
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "z z z\nb\n");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sub_requires_an_anchor_and_rejects_multiline_new() {
        let store = HashStore::new();
        let p = tmpfile("sub-bad");
        std::fs::write(&p, "a\n").unwrap();
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Sub {
                    line: None,
                    hash: None,
                    old: "a".into(),
                    new: "b".into(),
                    count: None,
                }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("needs a `line` or a `hash`"), "got: {}", err.message);

        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Sub {
                    line: Some(1),
                    hash: None,
                    old: "a".into(),
                    new: "b\nc".into(),
                    count: None,
                }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("line break"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\n");
        std::fs::remove_file(&p).ok();
    }

    /// A dry run validates everything and reports the would-be change without
    /// touching the file — the difference between hoping an edit landed and
    /// knowing what it would do.
    #[test]
    fn dry_run_reports_a_diff_and_writes_nothing() {
        let store = HashStore::new();
        let p = tmpfile("dry-run");
        std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
        store.snapshot(&p).unwrap();
        let out = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: true,
                ops: vec![Op::SetN {
                    line: 2,
                    content: "TWO".into(),
                }],
            },
        )
        .unwrap();
        assert!(out.starts_with("DRY RUN"), "got: {out}");
        assert!(out.contains("+1/-1"), "got: {out}");
        assert!(out.contains("-two"), "diff must show the old line: {out}");
        assert!(out.contains("+TWO"), "diff must show the new line: {out}");
        assert!(out.contains("1 unchanged"), "got: {out}");
        // Nothing was written and no rev was consumed.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(store.current_rev(&store.canonicalize(&p)), Some(0));
        std::fs::remove_file(&p).ok();
    }

    /// A dry run must still reject a bad batch: it is a validation pass, not a
    /// lint.
    #[test]
    fn dry_run_still_validates() {
        let store = HashStore::new();
        let p = tmpfile("dry-run-bad");
        std::fs::write(&p, "a\n").unwrap();
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: true,
                ops: vec![Op::SetN {
                    line: 9,
                    content: "x".into(),
                }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("out of bounds"), "got: {}", err.message);
        std::fs::remove_file(&p).ok();
    }

    /// The safety net for an agent-driven editor: a bad edit is one call away
    /// from being undone, without reaching for `git`.
    #[test]
    fn revert_restores_the_previous_content() {
        let store = HashStore::new();
        let p = tmpfile("revert");
        std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
        store.snapshot(&p).unwrap();
        apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::DeleteN { start: 1, end: 3 }],
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");

        let out = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Revert],
            },
        )
        .unwrap();
        assert!(out.contains("reverted"), "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\nthree\n");

        // The undo slot is consumed: a second revert says so.
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Revert],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("nothing to revert"), "got: {}", err.message);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn revert_must_stand_alone() {
        let store = HashStore::new();
        let p = tmpfile("revert-mixed");
        std::fs::write(&p, "a\n").unwrap();
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Revert, Op::SetN { line: 1, content: "b".into() }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("only op"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\n");
        std::fs::remove_file(&p).ok();
    }

    /// A hash shared by two identical lines is content-addressing working as
    /// designed: the anchor is genuinely ambiguous, so say so instead of
    /// silently picking one. Line-number ops are the escape hatch.
    #[test]
    fn ambiguous_hash_is_reported_with_the_candidate_lines() {
        let store = HashStore::new();
        let p = tmpfile("ambiguous");
        std::fs::write(&p, "same\nsame\nsame\n").unwrap();
        let h = read_hashes(&store, &p);
        assert_eq!(h[0], h[1], "identical lines must hash identically");
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Set {
                    hash: h[0].clone(),
                    line: None,
                    content: "x".into(),
                }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("ambiguous"), "got: {}", err.message);
        assert!(err.message.contains("1, 2, 3"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "same\nsame\nsame\n");
        std::fs::remove_file(&p).ok();
    }

    /// A file without a trailing newline must stay that way: adding one is a
    /// diff on the last line the caller never asked for.
    #[test]
    fn missing_trailing_newline_is_preserved() {
        let store = HashStore::new();
        let p = tmpfile("no-trailing-nl");
        std::fs::write(&p, "a\nb\nc").unwrap();
        apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::SetN {
                    line: 2,
                    content: "B".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nc");
        std::fs::remove_file(&p).ok();
    }

    /// Line ops on an empty file have nothing to address; the message must say
    /// that rather than reporting a nonsensical range.
    #[test]
    fn line_ops_on_an_empty_file_say_so() {
        let store = HashStore::new();
        let p = tmpfile("empty-lineno");
        std::fs::write(&p, "").unwrap();
        for ops in [
            vec![Op::SetN {
                line: 1,
                content: "x".into(),
            }],
            vec![Op::DeleteN { start: 1, end: 1 }],
        ] {
            let err = apply(
                &store,
                &Args {
                    path: p.display().to_string(),
                    rev: None,
                    dry_run: false,
                    ops,
                },
            )
            .unwrap_err();
            assert!(err.message.contains("empty"), "got: {}", err.message);
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
        std::fs::remove_file(&p).ok();
    }

    /// A hash alone cannot address one of several identical lines. Pairing it
    /// with the line number makes the anchor unambiguous without giving up the
    /// content check the hash provides.
    #[test]
    fn a_line_number_disambiguates_a_duplicate_hash() {
        let store = HashStore::new();
        let p = tmpfile("composite");
        std::fs::write(&p, "same\nsame\nsame\n").unwrap();
        let h = read_hashes(&store, &p);
        apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Set {
                    hash: h[1].clone(),
                    line: Some(2),
                    content: "two".into(),
                }],
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "same\ntwo\nsame\n");
        std::fs::remove_file(&p).ok();
    }

    /// Pairing a hash with a line it is *not* on means the read is stale —
    /// exactly the case a bare hash would have silently resolved elsewhere.
    #[test]
    fn a_mismatched_hash_and_line_fails_loudly() {
        let store = HashStore::new();
        let p = tmpfile("composite-bad");
        std::fs::write(&p, "alpha\nbeta\n").unwrap();
        let h = read_hashes(&store, &p);
        let err = apply(
            &store,
            &Args {
                path: p.display().to_string(),
                rev: None,
                dry_run: false,
                ops: vec![Op::Set {
                    hash: h[0].clone(),
                    line: Some(2),
                    content: "x".into(),
                }],
            },
        )
        .unwrap_err();
        assert!(err.message.contains("not on line"), "got: {}", err.message);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "alpha\nbeta\n");
        std::fs::remove_file(&p).ok();
    }
}
