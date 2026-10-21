//! Batch operation envelope for `hashline_edit`.
//!
//! Two addressing families, serialized with an `op` discriminator:
//! * hash ops — `set` / `insert` / `delete` / `replace`, anchored by 4-char
//!   `hashline_read` tags (`"HEAD"` prepends for `insert`). Content-addressed:
//!   identical lines can collide (`ambiguous`), tags shift every edit.
//! * line ops — `set_n` / `insert_n` / `delete_n` / `replace_n`, anchored by
//! * line ops — `set_n` / `insert_n` / `delete_n` / `replace_n`, anchored by
//!   absolute 1-based line numbers from any `hashline_read` (quiet output has
//!   them too). Preferred when you already know the numbers: no tag to copy,
//!   nothing ambiguous. Caveat: a stale line number silently edits the wrong
//!   line, while a stale rev/hash fails loudly — pass `rev` from your last
//!   read when the file may be contested (another writer, long-lived numbers).
//!
//! Every op validates against one pre-write snapshot before anything touches
//! disk. `rev` is optional: when present it is checked (stale-rev error on
//! mismatch); when omitted the snapshot inside the locked transaction is
//! authoritative.

use harness_contracts::ToolError;

/// Single batch operation.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(crate) enum Op {
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
    Replace {
        start_hash: String,
        end_hash: String,
        lines: Vec<String>,
    },
    /// Replace one line by number (1-based). No hash, no rev dance.
    #[serde(rename = "set_n")]
    SetN { line: u32, content: String },
    /// Insert after a line number; `after: 0` prepends.
    #[serde(rename = "insert_n")]
    InsertN { after: u32, lines: Vec<String> },
    /// Delete an inclusive line-number range.
    #[serde(rename = "delete_n")]
    DeleteN { start: u32, end: u32 },
    /// Replace an inclusive line-number range.
    #[serde(rename = "replace_n")]
    ReplaceN {
        start: u32,
        end: u32,
        lines: Vec<String>,
    },
    /// Substring replacement in one line, addressed by either family.
    ///
    /// `old` must occur exactly `count` times (default 1) in the line; this is
    /// the op for "change three characters in a 200-character line" without
    /// re-emitting the whole line and risking a typo elsewhere in it.
    Sub {
        #[serde(default)]
        line: Option<u32>,
        #[serde(default)]
        hash: Option<String>,
        old: String,
        new: String,
        #[serde(default)]
        count: Option<u32>,
    },
    Create {
        lines: Vec<String>,
    },
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct Args {
    pub path: String,
    #[serde(default)]
    pub rev: Option<u64>,
    pub ops: Vec<Op>,
}

/// Top-level keys accepted by `hashline_edit`.
const TOP_KEYS: &[&str] = &["path", "rev", "ops"];
/// Ops that write lines (so `lines` and `content` are meaningful).
const WRITE_KEYS: &[&str] = &["op", "lines", "content"];
const SET_KEYS: &[&str] = &["op", "hash", "content"];
const RANGE_KEYS: &[&str] = &["op", "start_hash", "end_hash"];
const RANGE_WRITE_KEYS: &[&str] = &["op", "start_hash", "end_hash", "lines"];
const INSERT_KEYS: &[&str] = &["op", "after_hash", "lines"];
const SET_N_KEYS: &[&str] = &["op", "line", "content"];
const RANGE_N_KEYS: &[&str] = &["op", "start", "end"];
const RANGE_N_WRITE_KEYS: &[&str] = &["op", "start", "end", "lines"];
const INSERT_N_KEYS: &[&str] = &["op", "after", "lines"];
/// `sub` anchors by line number *or* hash, so it accepts either.
const SUB_KEYS: &[&str] = &["op", "line", "hash", "old", "new", "count"];

const CREATE_KEYS: &[&str] = WRITE_KEYS;

/// The key set an op of this name may carry.
fn allowed_keys(op: &str) -> &'static [&'static str] {
    match op {
        "set" => SET_KEYS,
        "insert" => INSERT_KEYS,
        "delete" => RANGE_KEYS,
        "replace" => RANGE_WRITE_KEYS,
        "set_n" => SET_N_KEYS,
        "insert_n" => INSERT_N_KEYS,
        "delete_n" => RANGE_N_KEYS,
        "replace_n" => RANGE_N_WRITE_KEYS,
        "sub" => SUB_KEYS,
        "create" => CREATE_KEYS,
        // Unknown op: leave the diagnosis to the real parse, which names it.
        _ => &["op"],
    }
}

pub(crate) fn parse_args(args: &str) -> Result<Args, ToolError> {
    // Reject unknown keys instead of silently dropping them: a typo'd `revs`
    // or a `start_line` used to parse into something that quietly did less
    // than the caller meant.
    reject_unknown_keys(args)?;
    let a: Args = serde_json::from_str(args)
        .map_err(|e| crate::tool_err(format!("invalid arguments: {e}")))?;
    if a.path.trim().is_empty() {
        return Err(crate::tool_err("path must not be empty"));
    }
    if a.ops.is_empty() {
        return Err(crate::tool_err("ops must not be empty"));
    }
    Ok(a)
}

/// Structural check that no op carries a key from the other addressing family.
///
/// serde's internally-tagged enum forbids `deny_unknown_fields`, so this is
/// the equivalent guard for the op objects.
fn reject_unknown_keys(args: &str) -> Result<(), ToolError> {
    let v: serde_json::Value = serde_json::from_str(args)
        .map_err(|e| crate::tool_err(format!("invalid arguments: {e}")))?;
    let obj = v
        .as_object()
        .ok_or_else(|| crate::tool_err("arguments must be a JSON object"))?;
    for k in obj.keys() {
        if !TOP_KEYS.contains(&k.as_str()) {
            return Err(crate::tool_err(format!(
                "unknown argument {k:?}; expected one of {TOP_KEYS:?}"
            )));
        }
    }
    let Some(ops) = obj.get("ops").and_then(|o| o.as_array()) else {
        return Ok(()); // shape errors are reported by the real parse
    };
    for (i, op) in ops.iter().enumerate() {
        let Some(op) = op.as_object() else { continue };
        let name = op.get("op").and_then(|o| o.as_str()).unwrap_or("");
        let allowed = allowed_keys(name);
        for k in op.keys() {
            if !allowed.contains(&k.as_str()) {
                return Err(crate::tool_err(format!(
                    "op[{i}] ({name:?}) does not take {k:?}; allowed: {allowed:?}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_nine_ops() {
        let a = parse_args(
            r#"{"path":"f","rev":0,"ops":[
                {"op":"set","hash":"aaaa","content":"x"},
                {"op":"insert","after_hash":"HEAD","lines":["y"]},
                {"op":"delete","start_hash":"aaaa","end_hash":"bbbb"},
                {"op":"replace","start_hash":"aaaa","end_hash":"bbbb","lines":["z"]},
                {"op":"set_n","line":3,"content":"x"},
                {"op":"insert_n","after":0,"lines":["y"]},
                {"op":"delete_n","start":2,"end":4},
                {"op":"replace_n","start":2,"end":4,"lines":["z"]},
                {"op":"create","lines":["n"]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(a.ops.len(), 9);
        assert!(matches!(a.ops[3], Op::Replace { .. }));
        assert!(matches!(a.ops[4], Op::SetN { .. }));
        assert!(matches!(a.ops[8], Op::Create { .. }));
    }

    #[test]
    fn rev_is_optional() {
        let a = parse_args(r#"{"path":"f","ops":[{"op":"set_n","line":1,"content":"x"}]}"#).unwrap();
        assert!(a.rev.is_none());
        let a = parse_args(
            r#"{"path":"f","rev":2,"ops":[{"op":"set_n","line":1,"content":"x"}]}"#,
        )
        .unwrap();
        assert_eq!(a.rev, Some(2));
    }

    #[test]
    fn rejects_empty_path_ops_and_unknown_op() {
        assert!(parse_args(r#"{"path":"","rev":0,"ops":[]}"#).is_err());
        assert!(parse_args(r#"{"path":"f","rev":0,"ops":[]}"#).is_err());
        assert!(parse_args(r#"{"path":"f","rev":0,"ops":[{"op":"nuke"}]}"#).is_err());
    }

    #[test]
    fn rejects_unknown_keys_at_both_levels() {
        // Top level.
        assert!(parse_args(r#"{"path":"f","revs":0,"ops":[{"op":"set_n","line":1,"content":"x"}]}"#).is_err());
        // An op carrying a key from the other addressing family. Silent
        // acceptance here is how a `start_line` typo became a no-op.
        let err = parse_args(
            r#"{"path":"f","ops":[{"op":"delete_n","start":1,"end":2,"lines":["x"]}]}"#,
        )
        .unwrap_err();
        assert!(err.message.contains("does not take"), "got: {}", err.message);
        assert!(parse_args(
            r#"{"path":"f","ops":[{"op":"set","hash":"aaaa","content":"x","line":3}]}"#,
        )
        .is_err());
    }

    #[test]
    fn accepted_shapes_still_pass_the_guard() {
        assert!(parse_args(
            r#"{"path":"f","rev":1,"ops":[
                {"op":"insert","after_hash":"HEAD","lines":["y"]},
                {"op":"create","lines":["n"]},
                {"op":"insert_n","after":0,"lines":["z"]}
            ]}"#,
        )
        .is_ok());
    }
}
