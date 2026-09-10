//! Batch operation envelope for `hashline_edit`.
//!
//! Five ops, serialized with an `op` discriminator for compactness:
//! `{"op":"set","hash":"aB1c","content":"..."}` (single line only)
//! `{"op":"insert","after_hash":"aB1c","lines":[...]}` (`"HEAD"` prepends)
//! `{"op":"delete","start_hash":"aB1c","end_hash":"dE2f"}`
//! `{"op":"replace","start_hash":"aB1c","end_hash":"dE2f","lines":[...]}` —
//! multi-line replacement in one op
//! `{"op":"create","lines":[...]}` — new file only, fails if it exists
//!
//! All hashes resolve against one pre-write snapshot; every op is validated
//! before anything touches disk.

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
    Create {
        lines: Vec<String>,
    },
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct Args {
    pub path: String,
    pub rev: u64,
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
    fn parses_all_five_ops() {
        let a = parse_args(
            r#"{"path":"f","rev":0,"ops":[
                {"op":"set","hash":"aaaa","content":"x"},
                {"op":"insert","after_hash":"HEAD","lines":["y"]},
                {"op":"delete","start_hash":"aaaa","end_hash":"bbbb"},
                {"op":"replace","start_hash":"aaaa","end_hash":"bbbb","lines":["z"]},
                {"op":"create","lines":["n"]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(a.ops.len(), 5);
        assert!(matches!(a.ops[3], Op::Replace { .. }));
        assert!(matches!(a.ops[4], Op::Create { .. }));
    }

    #[test]
    fn rejects_empty_path_ops_and_unknown_op() {
        assert!(parse_args(r#"{"path":"","rev":0,"ops":[]}"#).is_err());
        assert!(parse_args(r#"{"path":"f","rev":0,"ops":[]}"#).is_err());
        assert!(parse_args(r#"{"path":"f","rev":0,"ops":[{"op":"nuke"}]}"#).is_err());
    }
}
