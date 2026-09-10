//! Argument envelope for `hashline_read`: path plus 1-based pagination.
//!
//! `offset` selects the first line rendered (default 1); `limit` caps how
//! many lines follow it (default: to EOF). Pagination only affects output —
//! the whole file is still hashed so the returned `REV` stays authoritative.

use std::path::PathBuf;

use harness_contracts::ToolError;

pub(crate) struct ReadArgs {
    pub path: PathBuf,
    pub offset: u64,
    pub limit: Option<u64>,
}

pub(crate) fn parse_args(args: &str) -> Result<ReadArgs, ToolError> {
    #[derive(serde::Deserialize)]
    struct Raw {
        path: String,
        #[serde(default)]
        offset: Option<u64>,
        #[serde(default)]
        limit: Option<u64>,
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
    if let Some(0) = raw.limit {
        return Err(crate::tool_err("limit must be >= 1"));
    }
    Ok(ReadArgs {
        path: PathBuf::from(raw.path),
        offset,
        limit: raw.limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_read_whole_file() {
        let a = parse_args(r#"{"path":"f.txt"}"#).unwrap();
        assert_eq!(a.offset, 1);
        assert_eq!(a.limit, None);
    }

    #[test]
    fn rejects_empty_path_zero_offset_and_zero_limit() {
        assert!(parse_args(r#"{"path":""}"#).is_err());
        assert!(parse_args(r#"{"path":"f","offset":0}"#).is_err());
        assert!(parse_args(r#"{"path":"f","limit":0}"#).is_err());
        assert!(parse_args(r#"{"path":"f","offset":2,"limit":3}"#).is_ok());
    }
}
