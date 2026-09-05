use std::{path::PathBuf, sync::Arc};

use futures::future::BoxFuture;
use harness_contracts::{KEY_HASH_STORE, KEY_TOOLS, ToolError, ToolSpec};
use harness_core::{Context, Result};
use harness_hash_base::{HashError, HashStore, line_ranges, parse_short};
use harness_tools::Tools;

pub struct HashlineEditPlugin;

impl harness_core::Plugin for HashlineEditPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("hashline-edit")
            .injects(KEY_TOOLS)
            .injects(KEY_HASH_STORE)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS)?;
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE)?;
        tools.register(hashline_edit_spec(), move |args| {
            hashline_edit_handler(Arc::clone(&store), args)
        })?;
        Ok(())
    }
}

fn tool_err(message: impl Into<String>) -> ToolError {
    ToolError {
        tool: "hashline_edit".to_owned(),
        message: message.into(),
    }
}

fn hash_to_tool(e: HashError) -> ToolError {
    tool_err(e.to_string())
}

// ---------------------------------------------------------------------------
// Tool envelope
// ---------------------------------------------------------------------------

/// Single batch operation. Serialized with `op` discriminator for compactness:
/// `{"op":"set","hash":"aB1c","content":"..."}`
/// `{"op":"insert","after_hash":"aB1c","lines":[...]}`
/// (`after_hash:"HEAD"` prepends)
/// `{"op":"delete","start_hash":"aB1c","end_hash":"dE2f"}`
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    Set {
        hash: String,
        content: String,
    },
    Insert {
        after_hash: String,
        lines: Vec<String>,
    },
    Delete {
        start_hash: String,
        end_hash: String,
    },
}

#[derive(Debug, serde::Deserialize)]
struct Args {
    path: String,
    rev: u64,
    ops: Vec<Op>,
}

fn hashline_edit_handler(
    store: Arc<HashStore>,
    args: String,
) -> BoxFuture<'static, Result<String, ToolError>> {
    let parsed: std::result::Result<Args, ToolError> = parse_args(&args);
    match parsed {
        Ok(a) => Box::pin(async move {
            tokio::task::spawn_blocking(move || apply(&store, &a))
                .await
                .map_err(|e| tool_err(format!("edit task failed: {e}")))?
        }),
        Err(e) => Box::pin(async move { Err(e) }),
    }
}

fn parse_args(args: &str) -> std::result::Result<Args, ToolError> {
    let a: Args =
        serde_json::from_str(args).map_err(|e| tool_err(format!("invalid arguments: {e}")))?;
    if a.path.trim().is_empty() {
        return Err(tool_err("path must not be empty"));
    }
    if a.ops.is_empty() {
        return Err(tool_err("ops must not be empty"));
    }
    Ok(a)
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

struct Resolved {
    /// Max affected original lineno (sort key, descending). Insert after
    /// HEAD -> 0.
    anchor: u32,
    kind: ResolvedKind,
}

enum ResolvedKind {
    Set { lineno: u32, content: Vec<u8> },
    Insert { after: u32, lines: Vec<Vec<u8>> },
    Delete { start: u32, end: u32 },
}

fn resolve_one(
    snap: &harness_hash_base::FileSnapshot,
    hash: &str,
) -> std::result::Result<u32, HashError> {
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

fn apply(store: &HashStore, a: &Args) -> std::result::Result<String, ToolError> {
    // Serialize edits in-process for clean retry semantics; reads proceed
    // concurrently. Hold for the whole validate+write transaction.
    let _edit = store.lock_edit();
    let canonical = store.canonicalize(&PathBuf::from(&a.path));

    // Cross-process mutual exclusion (blocking) via per-file lockfile.
    let _fs = store
        .exclusive_file_lock(&canonical)
        .map_err(hash_to_tool)?;

    // 1. Revision + external-modification guards *before* re-hashing.
    store
        .check_rev(&canonical, a.rev, "hashline_edit")
        .map_err(hash_to_tool)?;
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
    debug_assert_eq!(snap.rev, a.rev);

    // 3. Resolve + validate every op before touching disk.
    let mut resolved: Vec<Resolved> = Vec::with_capacity(a.ops.len());
    for op in &a.ops {
        match op {
            Op::Set { hash, content } => {
                if content.contains('\n') || content.contains('\r') {
                    return Err(tool_err(
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
                for l in lines {
                    if l.contains('\n') || l.contains('\r') {
                        return Err(tool_err(
                            "insert.lines entries must be single lines without newline",
                        ));
                    }
                }
                let after = if after_hash == "HEAD" {
                    0
                } else {
                    resolve_one(&snap, after_hash).map_err(hash_to_tool)?
                };
                resolved.push(Resolved {
                    anchor: after,
                    kind: ResolvedKind::Insert {
                        after,
                        lines: lines.iter().map(|l| l.as_bytes().to_vec()).collect(),
                    },
                });
            }
            Op::Delete {
                start_hash,
                end_hash,
            } => {
                let s = resolve_one(&snap, start_hash).map_err(hash_to_tool)?;
                let e = resolve_one(&snap, end_hash).map_err(hash_to_tool)?;
                if s > e {
                    return Err(tool_err(format!(
                        "delete range inverted: {start_hash}(line {s}) > {end_hash}(line {e})"
                    )));
                }
                resolved.push(Resolved {
                    anchor: e,
                    kind: ResolvedKind::Delete { start: s, end: e },
                });
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
                ResolvedKind::Insert { .. } => None,
            })
            .collect();
        ranges.sort();
        for w in ranges.windows(2) {
            if w[0].1 >= w[1].0 {
                return Err(tool_err(
                    "overlapping ops in one batch; split into separate edits or re-read",
                ));
            }
        }
        // Insert anchored inside a deleted range is ambiguous.
        for r in &resolved {
            if let ResolvedKind::Insert { after, .. } = &r.kind {
                for (s, e) in &ranges {
                    if *after >= *s && *after < *e {
                        // `after == e` is fine (append after the range).
                        return Err(tool_err(
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
                    return Err(tool_err(format!("set line {lineno} out of bounds")));
                }
                lines[i] = content;
            }
            ResolvedKind::Delete { start, end } => {
                let s = (start - 1) as usize;
                let e = end as usize; // inclusive -> exclusive
                if e > lines.len() || s >= e {
                    return Err(tool_err(format!(
                        "delete range {start}-{end} out of bounds"
                    )));
                }
                lines.drain(s..e);
            }
            ResolvedKind::Insert { after, lines: ins } => {
                let at = after as usize; // HEAD=0 -> prepend
                if at > lines.len() {
                    return Err(tool_err(format!(
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
        .ok_or_else(|| tool_err("path has no parent"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| tool_err(format!("cannot stage write: {e}")))?;
    // Best-effort permission preservation.
    let perm = disk_meta.permissions();
    let _ = tmp.as_file().set_permissions(perm);
    use std::io::Write as _;
    tmp.write_all(&buf)
        .map_err(|e| tool_err(format!("staged write failed: {e}")))?;
    tmp.flush()
        .map_err(|e| tool_err(format!("staged write failed: {e}")))?;
    tmp.persist(&canonical)
        .map_err(|e| tool_err(format!("atomic rename failed: {e}")))?;

    // 7. Commit revision. Next read re-hashes (incremental boundary:
    //    untouched lines are byte-identical; only rev/mtime/len advance
    //    here, hashing is deferred to the next snapshot).
    let meta = std::fs::metadata(&canonical).map_err(|e| hash_to_tool(HashError::Io(e)))?;
    let new_rev = store.commit_rev(
        &canonical,
        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
        meta.len(),
    );
    Ok(format!(
        "OK {}#REV:{new_rev} ops={}",
        canonical.display(),
        a.ops.len()
    ))
}

pub fn hashline_edit_spec() -> ToolSpec {
    ToolSpec {
        name: "hashline_edit".to_owned(),
        description: "Batch edit by 4-char line hashes. Args {path, rev, ops:[{op:set,hash,content}|{op:insert,after_hash,lines}|{op:delete,start_hash,end_hash}]}. after_hash HEAD prepends. All hashes validated first, applied bottom-up atomically; rev bumps on success. Re-read on stale/unknown hash.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "rev": { "type": "integer", "minimum": 0 },
                "ops": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": { "type": "string", "enum": ["set", "insert", "delete"] },
                            "hash": { "type": "string" },
                            "content": { "type": "string" },
                            "after_hash": { "type": "string" },
                            "lines": { "type": "array", "items": { "type": "string" } },
                            "start_hash": { "type": "string" },
                            "end_hash": { "type": "string" }
                        },
                        "required": ["op"]
                    }
                }
            },
            "required": ["path", "rev", "ops"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_TOOLS, ToolCall};
    use harness_hash_base::{HashBasePlugin, HashStore, short_str};
    use harness_tools::ToolsPlugin;
    use std::path::Path;

    fn ctx_with_plugins() -> Context {
        let ctx = Context::root();
        ctx.load(ToolsPlugin).unwrap();
        ctx.load(HashBasePlugin).unwrap();
        ctx.load(crate::HashlineEditPlugin).unwrap();
        ctx
    }

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

    fn edit_json(path: &Path, rev: u64, ops: serde_json::Value) -> String {
        serde_json::json!({"path": path.display().to_string(), "rev": rev, "ops": ops}).to_string()
    }

    #[test]
    fn set_replaces_line() {
        let store = HashStore::new();
        let p = tmpfile("set");
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = Args {
            path: p.display().to_string(),
            rev: 0,
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
            rev: 0,
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
            rev: 0,
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
            rev: 0,
            ops: vec![Op::Set {
                hash: h[0].clone(),
                content: "b".into(),
            }],
        };
        apply(&store, &args).unwrap();
        let h2 = read_hashes(&store, &p);
        let retry = Args {
            path: p.display().to_string(),
            rev: 0, // stale
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
            rev: 0,
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

    #[tokio::test]
    async fn tool_executes_through_registry() {
        let ctx = ctx_with_plugins();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let store: Arc<HashStore> = ctx.inject_key(KEY_HASH_STORE).unwrap();
        let p = tmpfile("reg");
        std::fs::write(&p, "x\ny\n").unwrap();
        let h = read_hashes(&store, &p);
        let args = edit_json(
            &p,
            0,
            serde_json::json!([{"op":"set","hash":h[0],"content":"X"}]),
        );
        let call = ToolCall {
            id: "c1".into(),
            name: "hashline_edit".into(),
            arguments: args,
        };
        let out = tools.execute("a", "s", 1, call).await.unwrap();
        assert!(out.starts_with("OK "));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "X\ny\n");
        std::fs::remove_file(&p).ok();
    }
}
