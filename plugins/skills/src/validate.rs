//! Frontmatter parsing + Agent Skills validation (pure, no I/O).
//!
//! Minimal YAML-subset parser for `SKILL.md` frontmatter: flat `key:
//! value` scalars (plain, single- or double-quoted single-line) plus one
//! indented `metadata:` string map. Anything exotic (flow collections,
//! block scalars, anchors) is rejected with a clear error so the caller
//! can skip the skill without affecting its siblings.
//!
//! Rules mirror the Agent Skills specification: `name` (1–64 chars,
//! `[a-z0-9-]`, no leading/trailing/consecutive hyphens, must match the
//! skill directory), `description` (1–1024 chars, non-empty), optional
//! `license` / `compatibility` (1–500 chars) / `metadata` (string map) /
//! `allowed-tools` (opaque; stored, not enforced in v1).

use std::collections::{HashMap, HashSet};

use harness_contracts::{SkillDetail, SkillMeta};

/// Cap for the `SKILL.md` instruction body. Progressive disclosure keeps
/// bodies small; an oversized file is a skill-authoring error, not a
/// truncation.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Parses one `SKILL.md` document into a [`SkillDetail`].
/// `dir_name` is the skill directory's file name; `root` its canonical path.
pub fn parse_skill(
    dir_name: &str,
    root: &std::path::Path,
    text: &str,
) -> Result<SkillDetail, String> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let (front, body) = split_frontmatter(text)?;
    let meta = parse_frontmatter(&front)?;
    check_skill(dir_name, &meta)?;
    if body.len() > MAX_BODY_BYTES {
        return Err(format!(
            "SKILL.md body is {}B (max {MAX_BODY_BYTES}B)",
            body.len()
        ));
    }
    let body = body.trim_start_matches(['\r', '\n']).to_owned();
    Ok(SkillDetail {
        meta,
        body,
        root: root.to_owned(),
    })
}

/// Full validation: directory coupling plus [`check_meta`] field rules.
pub fn check_skill(dir_name: &str, meta: &SkillMeta) -> Result<(), String> {
    check_name(&meta.name)?;
    if meta.name != dir_name {
        return Err(format!(
            "skill name `{}` must match its directory `{dir_name}`",
            meta.name
        ));
    }
    check_meta(meta)
}

/// Field rules without directory coupling (for programmatic registration).
pub fn check_meta(meta: &SkillMeta) -> Result<(), String> {
    check_name(&meta.name)?;
    if meta.description.trim().is_empty() {
        return Err("skill description must not be empty".to_owned());
    }
    if meta.description.chars().count() > 1024 {
        return Err("skill description must be 1-1024 characters".to_owned());
    }
    if meta.license.as_ref().is_some_and(|l| l.trim().is_empty()) {
        return Err("skill license must not be empty".to_owned());
    }
    if let Some(c) = &meta.compatibility {
        let n = c.chars().count();
        if c.trim().is_empty() || n > 500 {
            return Err("skill compatibility must be 1-500 characters".to_owned());
        }
    }
    for k in meta.metadata.keys() {
        if k.trim().is_empty() {
            return Err("skill metadata keys must not be empty".to_owned());
        }
    }
    if meta
        .allowed_tools
        .as_ref()
        .is_some_and(|a| a.trim().is_empty())
    {
        return Err("skill allowed-tools must not be empty".to_owned());
    }
    Ok(())
}

/// Validates a skill name: 1–64 chars, `[a-z0-9-]`, no leading/trailing
/// hyphen, no consecutive hyphens.
pub fn check_name(name: &str) -> Result<(), String> {
    let n = name.chars().count();
    if n == 0 || n > 64 {
        return Err("skill name must be 1-64 characters".to_owned());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(
            "skill name may only contain lowercase letters, digits, and hyphens".to_owned(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("skill name must not start or end with a hyphen".to_owned());
    }
    if name.contains("--") {
        return Err("skill name must not contain consecutive hyphens".to_owned());
    }
    Ok(())
}

/// Splits `text` into frontmatter lines `(lineno, line)` plus the raw body
/// after the closing fence.
fn split_frontmatter(text: &str) -> Result<(Vec<(usize, String)>, String), String> {
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_matches([' ', '\t', '\r']) != "---" {
        return Err("SKILL.md must start with a `---` frontmatter fence".to_owned());
    }
    let mut front = Vec::new();
    let mut offset = first.len() + 1;
    for (lineno, line) in (2..).zip(lines) {
        if line.trim_matches([' ', '\t', '\r']) == "---"
            || line.trim_matches([' ', '\t', '\r']) == "..."
        {
            let start = (offset + line.len() + 1).min(text.len());
            return Ok((front, text[start..].to_owned()));
        }
        front.push((lineno, line.to_owned()));
        offset += line.len() + 1;
    }
    Err("SKILL.md frontmatter is unterminated (missing closing `---`)".to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Name,
    Description,
    License,
    Compatibility,
    AllowedTools,
}

impl Field {
    fn key(self) -> &'static str {
        match self {
            Field::Name => "name",
            Field::Description => "description",
            Field::License => "license",
            Field::Compatibility => "compatibility",
            Field::AllowedTools => "allowed-tools",
        }
    }

    /// Plain-scalar continuation lines fold into these fields only;
    /// identifiers must stay single-line.
    fn foldable(self) -> bool {
        matches!(self, Field::Description | Field::Compatibility)
    }
}

fn field_of(key: &str) -> Option<Field> {
    match key {
        "name" => Some(Field::Name),
        "description" => Some(Field::Description),
        "license" => Some(Field::License),
        "compatibility" => Some(Field::Compatibility),
        "allowed-tools" => Some(Field::AllowedTools),
        _ => None,
    }
}

/// Parses frontmatter lines into [`SkillMeta`]. Unknown top-level fields
/// are rejected (forward-compat rides on the `metadata` map instead).
fn parse_frontmatter(lines: &[(usize, String)]) -> Result<SkillMeta, String> {
    let mut values: HashMap<Field, String> = HashMap::new();
    let mut seen: HashSet<&'static str> = HashSet::new();
    let mut metadata: HashMap<String, String> = HashMap::new();
    let mut in_metadata = false;
    let mut meta_indent: Option<usize> = None;
    // Active fold target after a `description:`/`compatibility:` scalar.
    let mut folding: Option<Field> = None;

    for (lineno, line) in lines {
        let lineno = *lineno;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with('\t') {
            return Err(format!("line {lineno}: tabs are not allowed, use spaces"));
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent == 0 {
            in_metadata = false;
            folding = None;
            let colon = line.find(':').ok_or_else(|| {
                format!("line {lineno}: expected `key: value`, found `{trimmed}`")
            })?;
            let key = line[..colon].trim();
            let right = &line[colon + 1..];
            if key == "metadata" {
                if !seen.insert("metadata") {
                    return Err(format!("line {lineno}: duplicate `metadata` field"));
                }
                if !right.trim().is_empty() {
                    return Err(format!(
                        "line {lineno}: `metadata` must be a mapping, one `key: value` per indented line"
                    ));
                }
                in_metadata = true;
                meta_indent = None;
                continue;
            }
            let field = field_of(key).ok_or_else(|| {
                format!("line {lineno}: unknown field `{key}` (put extensions under `metadata:`)")
            })?;
            if !seen.insert(field.key()) {
                return Err(format!("line {lineno}: duplicate `{key}` field"));
            }
            let value = right.trim();
            if value.is_empty() {
                if !field.foldable() {
                    return Err(format!(
                        "line {lineno}: `{key}` must have a value on the same line"
                    ));
                }
                values.insert(field, String::new());
                folding = Some(field);
                continue;
            }
            let parsed = parse_scalar(right, lineno)?;
            values.insert(field, parsed);
            if field.foldable() {
                folding = Some(field);
            }
        } else if in_metadata {
            let colon = line.find(':').ok_or_else(|| {
                format!("line {lineno}: expected indented `key: value` under `metadata:`")
            })?;
            if meta_indent.is_none() {
                meta_indent = Some(indent);
            } else if Some(indent) != meta_indent {
                return Err(format!(
                    "line {lineno}: inconsistent indent in `metadata:` map (nested maps are unsupported)"
                ));
            }
            let key = line[..colon].trim();
            if key.is_empty() {
                return Err(format!("line {lineno}: bad `metadata:` key"));
            }
            let value = parse_scalar(&line[colon + 1..], lineno)?;
            if metadata.insert(key.to_owned(), value).is_some() {
                return Err(format!("line {lineno}: duplicate `metadata.{key}` key"));
            }
        } else if line[indent..].contains(':') {
            return Err(format!(
                "line {lineno}: unexpected `key: value` outside `metadata:` (quote any `:` in continuations)"
            ));
        } else if let Some(field) = folding {
            let slot = values.get_mut(&field).expect("fold target present");
            if !slot.is_empty() {
                slot.push(' ');
            }
            slot.push_str(trimmed);
        } else {
            return Err(format!("line {lineno}: unexpected indented line"));
        }
    }

    // Required-field errors point at EOF when the fence closed the block.
    let eof = lines.last().map(|(n, _)| n + 1).unwrap_or(2);
    let name = values
        .remove(&Field::Name)
        .ok_or_else(|| format!("line {eof}: missing required field `name`"))?;
    let description = values
        .remove(&Field::Description)
        .ok_or_else(|| format!("line {eof}: missing required field `description`"))?;
    Ok(SkillMeta {
        name,
        description,
        license: values.remove(&Field::License),
        compatibility: values.remove(&Field::Compatibility),
        metadata,
        allowed_tools: values.remove(&Field::AllowedTools),
    })
}

/// Parses one scalar value: single-line plain (with ` #` comments cut,
/// YAML-style), double-quoted (escapes), or single-quoted (`''` escape).
fn parse_scalar(raw: &str, lineno: usize) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(format!("line {lineno}: empty value"));
    }
    if let Some(rest) = s.strip_prefix('"') {
        return parse_double_quoted(rest, lineno);
    }
    if let Some(rest) = s.strip_prefix('\'') {
        return parse_single_quoted(rest, lineno);
    }
    // Plain scalar: cut ` #` comments (YAML-style), reject exotic leads.
    let unquoted = match s.find(" #") {
        Some(i) => s[..i].trim_end(),
        None => s,
    };
    if unquoted.is_empty() {
        return Err(format!("line {lineno}: empty value"));
    }
    if let Some(c) = unquoted.chars().next()
        && "{}[]&*!|>%@`".contains(c)
    {
        return Err(format!(
            "line {lineno}: unsupported YAML construct starting with `{c}` (quote the value)"
        ));
    }
    Ok(unquoted.to_owned())
}

fn parse_double_quoted(rest: &str, lineno: usize) -> Result<String, String> {
    let bytes = rest.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut closed: Option<usize> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                closed = Some(i + 1);
                break;
            }
            b'\\' => {
                i += 1;
                match bytes.get(i) {
                    Some(b'n') => out.push('\n'),
                    Some(b't') => out.push('\t'),
                    Some(b'r') => out.push('\r'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'"') => out.push('"'),
                    _ => {
                        return Err(format!("line {lineno}: unsupported escape"));
                    }
                }
                i += 1;
            }
            _ => {
                let ch = rest[i..]
                    .chars()
                    .next()
                    .ok_or_else(|| format!("line {lineno}: bad UTF-8 in quoted string"))?;
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    let end = closed.ok_or_else(|| format!("line {lineno}: unterminated double-quoted string"))?;
    let after = rest[end..].trim();
    if !after.is_empty() && !after.starts_with('#') {
        return Err(format!(
            "line {lineno}: unexpected text after quoted string"
        ));
    }
    if out.trim().is_empty() {
        return Err(format!("line {lineno}: empty value"));
    }
    Ok(out)
}

fn parse_single_quoted(rest: &str, lineno: usize) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    let mut idx = 0;
    let mut closed: Option<usize> = None;
    while let Some(c) = chars.next() {
        idx += c.len_utf8();
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                idx += 1;
                out.push('\'');
            } else {
                closed = Some(idx);
                break;
            }
        } else {
            out.push(c);
        }
    }
    let end = closed.ok_or_else(|| format!("line {lineno}: unterminated single-quoted string"))?;
    let after = rest[end..].trim();
    if !after.is_empty() && !after.starts_with('#') {
        return Err(format!(
            "line {lineno}: unexpected text after quoted string"
        ));
    }
    if out.trim().is_empty() {
        return Err(format!("line {lineno}: empty value"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(front: &str, body: &str) -> String {
        format!("---\n{front}---\n{body}")
    }

    fn parse(dir: &str, text: &str) -> Result<SkillDetail, String> {
        parse_skill(dir, std::path::Path::new("/skills"), text)
    }

    #[test]
    fn minimal_skill_parses() {
        let d = parse(
            "pdf",
            &skill("name: pdf\ndescription: Works with PDFs.\n", "Do things.\n"),
        )
        .unwrap();
        assert_eq!(d.meta.name, "pdf");
        assert_eq!(d.meta.description, "Works with PDFs.");
        assert_eq!(d.body, "Do things.\n");
        assert!(d.meta.metadata.is_empty());
    }

    #[test]
    fn empty_body_is_valid() {
        let d = parse("a", &skill("name: a\ndescription: Does a.\n", "")).unwrap();
        assert!(d.body.is_empty());
    }

    #[test]
    fn full_frontmatter_parses() {
        let d = parse(
            "pdf-processing",
            &skill(
                "name: pdf-processing\ndescription: Extract PDF text.\nlicense: Apache-2.0\ncompatibility: Requires poppler\nmetadata:\n  author: example-org\n  version: \"1.0\"\nallowed-tools: Read Bash\n",
                "Body.\n",
            ),
        )
        .unwrap();
        assert_eq!(d.meta.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(d.meta.compatibility.as_deref(), Some("Requires poppler"));
        assert_eq!(
            d.meta.metadata.get("author").map(String::as_str),
            Some("example-org")
        );
        assert_eq!(
            d.meta.metadata.get("version").map(String::as_str),
            Some("1.0")
        );
        assert_eq!(d.meta.allowed_tools.as_deref(), Some("Read Bash"));
    }

    #[test]
    fn missing_fence_is_rejected() {
        assert!(parse("a", "name: a\ndescription: x\n").is_err());
    }

    #[test]
    fn unterminated_frontmatter_is_rejected() {
        assert!(parse("a", "---\nname: a\n").is_err());
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        assert!(parse("a", &skill("name: a\n", "")).is_err());
        assert!(parse("a", &skill("description: x\n", "")).is_err());
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(parse("a", &skill("name: a\ndescription: x\nversion: 1\n", "")).is_err());
    }

    #[test]
    fn duplicate_field_is_rejected() {
        assert!(parse("a", &skill("name: a\nname: b\ndescription: x\n", "")).is_err());
    }

    #[test]
    fn bad_names_are_rejected() {
        for bad in [
            "",
            "A",
            "-a",
            "a-",
            "a--b",
            "a_b",
            "a b",
            "a/b",
            &"a".repeat(65),
        ] {
            let doc = skill(&format!("name: {bad}\ndescription: x\n"), "");
            assert!(parse("x", &doc).is_err(), "name {bad:?} accepted");
        }
    }

    #[test]
    fn name_must_match_dir() {
        let doc = skill("name: a\ndescription: x\n", "");
        assert!(parse("b", &doc).is_err());
    }

    #[test]
    fn empty_and_long_descriptions_rejected() {
        assert!(parse("a", &skill("name: a\ndescription: \n", "")).is_err());
        let long = "x".repeat(1025);
        assert!(parse("a", &skill(&format!("name: a\ndescription: {long}\n"), "")).is_err());
    }

    #[test]
    fn folded_continuation_joins_with_space() {
        let d = parse(
            "a",
            &skill("name: a\ndescription: First line\n  second line\n", ""),
        )
        .unwrap();
        assert_eq!(d.meta.description, "First line second line");
    }

    #[test]
    fn colon_continuation_must_be_quoted() {
        assert!(parse("a", &skill("name: a\ndescription: x\n  y: z\n", "")).is_err());
        let d = parse("a", &skill("name: a\ndescription: \"x: y\"\n", "")).unwrap();
        assert_eq!(d.meta.description, "x: y");
    }

    #[test]
    fn quoted_values_and_comments() {
        let d = parse(
            "a",
            &skill(
                "name: 'a' # comment\ndescription: \"does x\" # trailing\n",
                "",
            ),
        )
        .unwrap();
        assert_eq!(d.meta.name, "a");
        assert_eq!(d.meta.description, "does x");
    }

    #[test]
    fn long_compatibility_rejected() {
        let long = "x".repeat(501);
        assert!(
            parse(
                "a",
                &skill(
                    &format!("name: a\ndescription: x\ncompatibility: {long}\n"),
                    ""
                )
            )
            .is_err()
        );
    }

    #[test]
    fn bad_metadata_rejected() {
        // flow style unsupported
        assert!(
            parse(
                "a",
                &skill("name: a\ndescription: x\nmetadata: {a: b}\n", "")
            )
            .is_err()
        );
        // nested maps unsupported
        assert!(
            parse(
                "a",
                &skill("name: a\ndescription: x\nmetadata:\n  a: b\n    c: d\n", "")
            )
            .is_err()
        );
    }

    #[test]
    fn oversized_body_rejected() {
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(parse("a", &skill("name: a\ndescription: x\n", &big)).is_err());
    }

    #[test]
    fn single_quoted_escape() {
        let d = parse("a", &skill("name: a\ndescription: 'it''s x'\n", "")).unwrap();
        assert_eq!(d.meta.description, "it's x");
    }

    #[test]
    fn bom_is_stripped() {
        let doc = "\u{FEFF}---\nname: a\ndescription: x\n---\n".to_string();
        assert!(parse("a", &doc).is_ok());
    }
}
