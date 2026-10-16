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

pub(crate) fn parse_args(args: &str) -> Result<Args, ToolError> {
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
}
