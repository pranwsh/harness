use crate::{
    syntax,
    text::{self, Anchor, Document},
};
use serde::Deserialize;
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    pub path: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub ranges: Vec<Range>,
    pub symbol: Option<String>,
    pub around: Option<Around>,
    pub search: Option<Search>,
    #[serde(default)]
    pub outline: bool,
    pub anchors: Option<bool>,
    pub max_tokens: Option<usize>,
    pub max_line_width: Option<usize>,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Range {
    pub start: usize,
    pub end: usize,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Around {
    pub pattern: Option<String>,
    pub anchor: Option<Anchor>,
    #[serde(default = "five")]
    pub before: usize,
    #[serde(default = "five")]
    pub after: usize,
}
fn five() -> usize {
    5
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Search {
    pub regex: String,
    pub glob: Option<String>,
    #[serde(default)]
    pub context: usize,
    pub max_matches: Option<usize>,
    #[serde(default)]
    pub output: Output,
}
#[derive(Default, Deserialize, schemars::JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Output {
    #[default]
    Matches,
    Files,
    Count,
}

pub(crate) fn run(store: &harness_hash_base::HashStore, args: Args) -> Result<String, String> {
    let mut paths = args.paths.clone();
    if let Some(path) = &args.path {
        paths.insert(0, path.clone());
    }
    if paths.is_empty() || paths.len() > 64 || paths.iter().any(|p| p.trim().is_empty()) {
        return Err("provide path or paths (1–64 nonempty paths)".into());
    }
    let selectors = usize::from(!args.ranges.is_empty())
        + usize::from(args.symbol.is_some())
        + usize::from(args.around.is_some())
        + usize::from(args.search.is_some())
        + usize::from(args.outline);
    if selectors > 1 {
        return Err("choose one of ranges, symbol, around, search, outline".into());
    }
    if args.ranges.len() > 64 || args.ranges.iter().any(|r| r.start == 0 || r.end < r.start) {
        return Err("ranges must contain at most 64 inclusive 1-based {start,end} ranges".into());
    }
    let max_tokens = args.max_tokens.unwrap_or(4096);
    if !(64..=32768).contains(&max_tokens) {
        return Err("max_tokens must be between 64 and 32768".into());
    }
    let width = args.max_line_width.unwrap_or(300);
    if !(1..=10000).contains(&width) {
        return Err("max_line_width must be between 1 and 10000".into());
    }
    let anchors = args.anchors.unwrap_or(true);
    let mut output = String::new();
    let mut seen = BTreeSet::new();
    let regex = args
        .search
        .as_ref()
        .map(|s| {
            regex::RegexBuilder::new(&s.regex)
                .size_limit(2 * 1024 * 1024)
                .build()
                .map_err(|e| e.to_string())
        })
        .transpose()?;
    let glob = args
        .search
        .as_ref()
        .and_then(|s| s.glob.as_ref())
        .map(|s| {
            globset::Glob::new(s)
                .map(|g| g.compile_matcher())
                .map_err(|e| e.to_string())
        })
        .transpose()?;
    let mut files = Vec::new();
    for path in paths {
        let path = PathBuf::from(path);
        if path.is_dir() && args.search.is_some() {
            for entry in walkdir::WalkDir::new(&path)
                .follow_links(false)
                .max_depth(64)
            {
                let entry = entry.map_err(|e| e.to_string())?;
                if entry.file_type().is_file()
                    && glob.as_ref().is_none_or(|g| {
                        g.is_match(entry.path())
                            || g.is_match(entry.path().strip_prefix(&path).unwrap_or(entry.path()))
                    })
                {
                    files.push(entry.into_path());
                }
                if files.len() > 10000 {
                    return Err("search exceeds 10000 files; narrow paths/glob".into());
                }
            }
        } else {
            files.push(path);
        }
    }
    files.sort();
    let mut total_matches = 0usize;
    let max_matches = args
        .search
        .as_ref()
        .and_then(|s| s.max_matches)
        .unwrap_or(100);
    if max_matches == 0 || max_matches > 10000 {
        return Err("max_matches must be 1–10000".into());
    }
    let mut scanned_bytes = 0usize;
    for path in files {
        let canonical =
            std::fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !seen.insert(canonical.clone()) {
            continue;
        }
        if canonical.is_dir() {
            let mut entries = std::fs::read_dir(&canonical)
                .map_err(|e| e.to_string())?
                .map(|e| {
                    e.map(|e| {
                        format!(
                            "{}{}",
                            e.file_name().to_string_lossy(),
                            if e.path().is_dir() { "/" } else { "" }
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            entries.sort();
            output.push_str(&format!(
                "¶{} directory entries={}\n{}\n",
                canonical.display(),
                entries.len(),
                entries
                    .into_iter()
                    .take(1000)
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
            continue;
        }
        if glob.as_ref().is_some_and(|g| {
            !g.is_match(&path) && !g.is_match(path.file_name().unwrap_or_default())
        }) {
            continue;
        }
        let _guard = store.lock_edit(&canonical).map_err(|e| e.to_string())?;
        let bytes = match text::read_bytes(&canonical) {
            Ok(b) => b,
            Err(e) if args.search.is_some() => {
                output.push_str(&format!("[skipped {}: {e}]\n", path.display()));
                continue;
            }
            Err(e) => return Err(e),
        };
        scanned_bytes += bytes.len();
        if scanned_bytes > 128 * 1024 * 1024 {
            output.push_str("[overflow: 128 MiB scan budget reached; narrow paths/glob]\n");
            break;
        }
        let doc = match Document::decode(&bytes) {
            Ok(d) => d,
            Err(e) if args.search.is_some() => {
                output.push_str(&format!("[skipped {}: {e}]\n", path.display()));
                continue;
            }
            Err(e) => return Err(e),
        };
        let lines = doc.ranges().len();
        let mut selected = BTreeSet::new();
        let mut note = String::new();
        if let Some(search) = &args.search {
            let regex = regex.as_ref().expect("compiled search");
            let hits = doc
                .text
                .split_terminator('\n')
                .enumerate()
                .filter_map(|(i, line)| regex.is_match(line).then_some(i + 1))
                .collect::<Vec<_>>();
            if hits.is_empty() {
                continue;
            }
            let remaining = max_matches.saturating_sub(total_matches);
            let shown = hits.len().min(remaining);
            total_matches += shown;
            match search.output {
                Output::Files => output.push_str(&format!("{}\n", canonical.display())),
                Output::Count => {
                    output.push_str(&format!("{} matches={}\n", canonical.display(), hits.len()))
                }
                Output::Matches => {
                    for n in hits.iter().take(shown) {
                        add_context(&mut selected, *n, search.context, search.context, lines);
                    }
                    note = format!(" matches={} shown={shown}", hits.len());
                }
            }
            if hits.len() > shown {
                note.push_str(" [overflow: max_matches reached]");
            }
            if search.output != Output::Matches {
                if total_matches >= max_matches {
                    output
                        .push_str("[overflow: max_matches reached; remaining files not scanned]\n");
                    break;
                }
                continue;
            }
        } else if args.outline {
            output.push_str(&format!(
                "¶{} version={} lines={lines}\n",
                canonical.display(),
                text::version(&bytes)
            ));
            for symbol in syntax::symbols(&canonical, &doc.text)? {
                output.push_str(&format!(
                    "{} {}-{} {}\n",
                    symbol.name, symbol.start, symbol.end, symbol.kind
                ));
            }
            continue;
        } else if let Some(name) = &args.symbol {
            let symbols = syntax::symbols(&canonical, &doc.text)?;
            let hits = symbols
                .iter()
                .filter(|s| s.name == *name || s.name.rsplit('.').next() == Some(name.as_str()))
                .collect::<Vec<_>>();
            if hits.len() != 1 {
                return Err(format!(
                    "symbol {name:?}: {} candidates\n{}",
                    hits.len(),
                    symbols
                        .iter()
                        .map(|s| format!("{} {}-{}", s.name, s.start, s.end))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            selected.extend(hits[0].start..=hits[0].end);
        } else if let Some(around) = &args.around {
            if around.pattern.is_some() == around.anchor.is_some() {
                return Err("around requires exactly one of pattern or anchor".into());
            }
            if let Some(anchor) = &around.anchor {
                if doc.anchor(anchor.line).as_ref().map(|a| &a.hash) != Some(&anchor.hash) {
                    return Err(format!(
                        "anchor mismatch\n{}",
                        doc.rows(
                            anchor.line.saturating_sub(3).max(1)
                                ..=anchor.line.saturating_add(3).min(lines),
                            true,
                            width
                        )
                    ));
                }
                add_context(
                    &mut selected,
                    anchor.line,
                    around.before,
                    around.after,
                    lines,
                );
            } else if let Some(pattern) = &around.pattern {
                if pattern.is_empty() {
                    return Err("around pattern cannot be empty".into());
                }
                for (i, line) in doc.text.split_terminator('\n').enumerate() {
                    if line.contains(pattern) {
                        add_context(&mut selected, i + 1, around.before, around.after, lines);
                    }
                }
            }
        } else if !args.ranges.is_empty() {
            for range in &args.ranges {
                if range.start > lines {
                    return Err(format!(
                        "range starts beyond EOF: {} has {lines} lines",
                        canonical.display()
                    ));
                }
                selected.extend(range.start..=range.end.min(lines));
            }
        } else {
            selected.extend(1..=lines.min(50));
            if lines > 50 {
                note.push_str(" [overflow: default 50-line window; use ranges]");
            }
        }
        output.push_str(&format!(
            "¶{} version={} lines={lines}{note}\n",
            canonical.display(),
            text::version(&bytes)
        ));
        output.push_str(&doc.rows(selected, anchors, width));
        if args.search.is_some() && total_matches >= max_matches {
            output.push_str("[overflow: max_matches reached; remaining files not scanned]\n");
            break;
        }
        // Bound accumulation as well as the final response; one file is at most MAX_BYTES.
        if output.len() > max_tokens.saturating_mul(32) {
            output.push_str("[overflow: output budget reached; remaining paths not read]\n");
            break;
        }
    }
    if output.is_empty() {
        output.push_str("No matches.\n");
    }
    text::bounded(output, max_tokens)
}

fn add_context(out: &mut BTreeSet<usize>, n: usize, before: usize, after: usize, total: usize) {
    out.extend(n.saturating_sub(before).max(1)..=n.saturating_add(after).min(total));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_search_and_outline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("parser.py");
        std::fs::write(
            &p,
            "class Parser:\n    def parse_header(self):\n        pass\n\n# end\n",
        )
        .unwrap();
        let invoke = |extra: serde_json::Value| {
            let mut v = extra;
            v["path"] = serde_json::json!(p);
            run(
                &harness_hash_base::HashStore::new(),
                serde_json::from_value(v).unwrap(),
            )
            .unwrap()
        };
        let out = invoke(serde_json::json!({"ranges":[{"start":1,"end":1},{"start":5,"end":5}]}));
        assert!(out.contains("class Parser:"));
        assert!(out.contains("# end"));
        assert!(!out.contains("pass"));
        assert!(invoke(serde_json::json!({"outline":true})).contains("Parser.parse_header"));
        assert!(invoke(serde_json::json!({"symbol":"Parser.parse_header"})).contains("pass"));
        assert!(
            invoke(serde_json::json!({"search":{"regex":"def", "context":1}}))
                .contains("class Parser:")
        );
    }
}
