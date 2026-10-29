//! Argument envelope for `hashline_read`.
//!
//! Defaults are tuned for cheap reads: `offset` starts at 1 and `limit`
//! defaults to [`DEFAULT_LIMIT`] lines, so a bare `{"path": ...}` never dumps
//! a whole large file into context. Pass `limit: 0` (or any value `<= 0`) to
//! read to EOF.
//!
//! `hashes` opts into the edit-ready format (`{line}:{hash}|{content}` with a
//! `#REV` marker) used by `hashline_edit`; it is off by default, which renders
//! the quieter `{line}: {content}`.
//!
//! `if_rev_match` short-circuits: when the file's current revision already
//! equals the caller's, the body is elided and a compact `UNCHANGED` header is
//! returned. `grep` restricts output to lines containing a substring.

use std::path::PathBuf;

use harness_contracts::ToolError;

/// Lines returned when `limit` is omitted.
pub(crate) const DEFAULT_LIMIT: u64 = 50;

pub(crate) struct ReadArgs {
    pub path: PathBuf,
    pub offset: u64,
    /// `None` means "to EOF" (explicit `limit <= 0`).
    pub limit: Option<u64>,
    pub hashes: bool,
    pub if_rev_match: Option<u64>,
    pub grep: Option<String>,
    /// Case-insensitive substring match for `grep`.
    pub ignore_case: bool,
    /// Lines of context to include around each grep match.
    pub context: usize,
}

pub(crate) fn parse_args(args: &str) -> Result<ReadArgs, ToolError> {
    #[derive(serde::Deserialize)]
    // Unknown keys are rejected: `{"path":..., "hash":true}` used to be
    // silently ignored, which then surfaced as a mysterious `unknown hash`
    // one tool call later.
    #[serde(deny_unknown_fields)]
    struct Raw {
        path: String,
        #[serde(default)]
        offset: Option<u64>,
        #[serde(default)]
        limit: Option<i64>,
        #[serde(default)]
        hashes: Option<bool>,
        #[serde(default)]
        if_rev_match: Option<u64>,
        #[serde(default)]
        grep: Option<String>,
        #[serde(default)]
        ignore_case: Option<bool>,
        #[serde(default)]
        context: Option<u32>,
    }
    let raw: Raw = serde_json::from_str(args)
        .map_err(|e| crate::tool_err(format!("invalid arguments: {e}")))?;
    if raw.path.trim().is_empty() {
        return Err(crate::tool_err("path must not be empty"));
    }
    let offset = raw.offset.unwrap_or(1);
    if offset < 1 {
        return Err(crate::tool_err("offset is 1-based, must be >= 1"));
    }
    // `limit <= 0` means "to EOF"; omitted uses the default window.
    let limit = match raw.limit {
        None => Some(DEFAULT_LIMIT),
        Some(n) if n <= 0 => None,
        Some(n) => Some(n as u64),
    };
    let grep = raw.grep.filter(|g| !g.is_empty());
    Ok(ReadArgs {
        path: PathBuf::from(raw.path),
        offset,
        limit,
        // Hashes are on by default: they are what makes the output editable,
        // and needing a second read (or a failed edit) to get them costs more
        // than the four bytes per line they add.
        hashes: raw.hashes.unwrap_or(true),
        if_rev_match: raw.if_rev_match,
        grep,
        ignore_case: raw.ignore_case.unwrap_or(false),
        context: raw.context.unwrap_or(0) as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_a_fifty_line_hashed_window() {
        let a = parse_args(r#"{"path":"f.txt"}"#).unwrap();
        assert_eq!(a.offset, 1);
        assert_eq!(a.limit, Some(DEFAULT_LIMIT));
        // Hashes default on: they are what makes the rows editable.
        assert!(a.hashes);
        assert!(a.if_rev_match.is_none());
        assert!(a.grep.is_none());
        assert!(!a.ignore_case);
    }

    #[test]
    fn hashes_are_opt_out() {
        assert!(!parse_args(r#"{"path":"f","hashes":false}"#).unwrap().hashes);
    }

    #[test]
    fn unknown_arguments_are_rejected_not_ignored() {
        // A typo'd key used to be dropped silently, turning a hashed read into
        // a quiet one and surfacing later as `unknown hash`.
        assert!(parse_args(r#"{"path":"f","hash":true}"#).is_err());
        assert!(parse_args(r#"{"path":"f","nope":1}"#).is_err());
    }

    #[test]
    fn non_positive_limit_reads_to_eof() {
        assert!(parse_args(r#"{"path":"f","limit":0}"#).unwrap().limit.is_none());
        assert!(parse_args(r#"{"path":"f","limit":-1}"#).unwrap().limit.is_none());
        assert_eq!(parse_args(r#"{"path":"f","limit":7}"#).unwrap().limit, Some(7));
    }

    #[test]
    fn rejects_empty_path_and_zero_offset() {
        assert!(parse_args(r#"{"path":""}"#).is_err());
        assert!(parse_args(r#"{"path":"f","offset":0}"#).is_err());
    }

    #[test]
    fn parses_hashes_rev_grep_and_ignore_case() {
        let a = parse_args(
            r#"{"path":"f","hashes":true,"if_rev_match":3,"grep":"x","ignore_case":true}"#,
        )
        .unwrap();
        assert!(a.hashes);
        assert_eq!(a.if_rev_match, Some(3));
        assert_eq!(a.grep.as_deref(), Some("x"));
        assert!(a.ignore_case);
        // Empty grep is treated as absent.
        assert!(parse_args(r#"{"path":"f","grep":""}"#).unwrap().grep.is_none());
    }
}
