use crate::{
    syntax,
    text::{self, Anchor, Document},
};
use harness_hash_base::HashStore;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    #[serde(default)]
    pub edits: Vec<Edit>,
    pub undo: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "yes")]
    pub validate: bool,
    #[serde(default = "yes")]
    pub auto_rollback: bool,
    pub max_tokens: Option<usize>,
}
fn yes() -> bool {
    true
}
#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct Edit {
    pub path: String,
    pub expected_version: Option<String>,
    #[serde(flatten)]
    pub operation: Operation,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Operation {
    Replace {
        start: Anchor,
        end: Anchor,
        text: String,
    },
    InsertBefore {
        anchor: Anchor,
        text: String,
    },
    InsertAfter {
        anchor: Anchor,
        text: String,
    },
    Delete {
        start: Anchor,
        end: Anchor,
    },
    ReplaceText {
        old: String,
        new: String,
        occurrence: Option<usize>,
        #[serde(default)]
        all: bool,
    },
    Create {
        text: String,
    },
    Move {
        to: String,
    },
    DeleteFile,
}

#[derive(Clone)]
struct State {
    bytes: Option<Vec<u8>>,
    permissions: Option<fs::Permissions>,
}
#[derive(Clone)]
struct Undo {
    id: String,
    before: BTreeMap<PathBuf, State>,
    after: BTreeMap<PathBuf, State>,
}
#[derive(Default)]
pub(crate) struct Editor {
    history: Mutex<VecDeque<Undo>>,
}
struct Patch {
    start: usize,
    end: usize,
    text: String,
}

impl Editor {
    pub fn run(&self, store: &Arc<HashStore>, args: Args) -> Result<String, String> {
        let max_tokens = args.max_tokens.unwrap_or(4096);
        if !(64..=32768).contains(&max_tokens) {
            return Err("max_tokens must be 64–32768".into());
        }
        if let Some(id) = &args.undo {
            if !args.edits.is_empty() {
                return Err("undo cannot be combined with edits".into());
            }
            return self.undo(store, id, &args);
        }
        if args.edits.is_empty() || args.edits.len() > 256 {
            return Err("edits must contain 1–256 operations".into());
        }
        let mut paths = BTreeSet::new();
        for edit in &args.edits {
            paths.insert(canonical(store, &edit.path)?);
            if let Operation::Move { to } = &edit.operation {
                paths.insert(canonical(store, to)?);
            }
        }
        if paths.len() > 64 {
            return Err("at most 64 paths per transaction".into());
        }
        // The canonical path ordering is shared by all read/edit transactions.
        let _guards = paths
            .iter()
            .map(|p| store.lock_edit(p).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let before = paths
            .iter()
            .map(|p| load(p).map(|s| (p.clone(), s)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        if before
            .values()
            .map(|s| s.bytes.as_ref().map_or(0, Vec::len))
            .sum::<usize>()
            > 64 * 1024 * 1024
        {
            return Err("transaction exceeds 64 MiB input budget; split the batch".into());
        }
        let mut after = before.clone();
        let mut grouped: BTreeMap<PathBuf, Vec<&Edit>> = BTreeMap::new();
        for edit in &args.edits {
            grouped
                .entry(canonical(store, &edit.path)?)
                .or_default()
                .push(edit);
        }
        // Lifecycle targets cannot simultaneously receive another operation.
        let mut lifecycle = BTreeSet::new();
        for (path, edits) in &grouped {
            for edit in edits {
                match &edit.operation {
                    Operation::Create { .. } | Operation::DeleteFile | Operation::Move { .. } => {
                        if edits.len() != 1 || !lifecycle.insert(path.clone()) {
                            return Err(
                                "file lifecycle operations must be exclusive for each path".into(),
                            );
                        }
                        if let Operation::Move { to } = &edit.operation {
                            let target = canonical(store, to)?;
                            if target == *path
                                || grouped.contains_key(&target)
                                || !lifecycle.insert(target)
                            {
                                return Err("move target conflicts with another operation".into());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut notes = Vec::new();
        for (path, edits) in grouped {
            let original = &before[&path];
            for edit in &edits {
                if let Some(expected) = &edit.expected_version {
                    let actual = original
                        .bytes
                        .as_deref()
                        .map(text::version)
                        .unwrap_or_else(|| "missing".into());
                    if *expected != actual {
                        return Err(recovery(
                            &path,
                            original,
                            &format!("version mismatch: expected {expected}, current {actual}"),
                            &[],
                        ));
                    }
                }
            }
            match &edits[0].operation {
                Operation::Create { text } => {
                    if original.bytes.is_some() {
                        return Err(recovery(
                            &path,
                            original,
                            "create requires a missing path",
                            &[],
                        ));
                    }
                    if text.len() as u64 > text::MAX_BYTES {
                        return Err("created file exceeds 32 MiB".into());
                    }
                    let doc = Document::new(text.clone())?;
                    after.get_mut(&path).expect("known path").bytes = Some(doc.encode(&doc.text));
                }
                Operation::DeleteFile => {
                    if original.bytes.is_none() {
                        return Err(format!("{} is missing", path.display()));
                    }
                    after.get_mut(&path).expect("known path").bytes = None;
                }
                Operation::Move { to } => {
                    let target = canonical(store, to)?;
                    if original.bytes.is_none() || before[&target].bytes.is_some() {
                        return Err(
                            "move requires an existing source and a missing destination".into()
                        );
                    }
                    after.insert(target, original.clone());
                    after.get_mut(&path).expect("known path").bytes = None;
                }
                _ => {
                    let bytes = original
                        .bytes
                        .as_ref()
                        .ok_or_else(|| format!("{} is missing; use create", path.display()))?;
                    let doc = Document::decode(bytes)?;
                    let ranges = doc.ranges();
                    let mut patches = Vec::new();
                    for edit in edits {
                        match &edit.operation {
                            Operation::Replace { start, end, .. }
                            | Operation::Delete { start, end } => {
                                check_anchor(&path, original, &doc, start)?;
                                check_anchor(&path, original, &doc, end)?;
                                if start.line > end.line {
                                    return Err("range start exceeds end".into());
                                }
                                let lo = ranges[start.line - 1].start;
                                let hi = ranges[end.line - 1].end;
                                let text = if matches!(edit.operation, Operation::Delete { .. }) {
                                    String::new()
                                } else {
                                    line_text(
                                        text_for(&edit.operation).unwrap_or(""),
                                        hi < doc.text.len() || doc.text.ends_with('\n'),
                                    )
                                };
                                warn_indent(&doc.text[lo..hi], &text, &mut notes);
                                patches.push(Patch {
                                    start: lo,
                                    end: hi,
                                    text,
                                });
                            }
                            Operation::InsertBefore { anchor, text }
                            | Operation::InsertAfter { anchor, text } => {
                                check_anchor(&path, original, &doc, anchor)?;
                                let range = &ranges[anchor.line - 1];
                                let before_anchor =
                                    matches!(edit.operation, Operation::InsertBefore { .. });
                                let position = if before_anchor {
                                    range.start
                                } else {
                                    range.end
                                };
                                let mut inserted = line_text(
                                    text,
                                    before_anchor
                                        || position < doc.text.len()
                                        || doc.text.ends_with('\n'),
                                );
                                if !before_anchor
                                    && position == doc.text.len()
                                    && !doc.text.ends_with('\n')
                                {
                                    inserted.insert(0, '\n');
                                }
                                warn_indent(&doc.text[range.clone()], &inserted, &mut notes);
                                patches.push(Patch {
                                    start: position,
                                    end: position,
                                    text: inserted,
                                });
                            }
                            Operation::ReplaceText {
                                old,
                                new,
                                occurrence,
                                all,
                            } => {
                                if old.is_empty()
                                    || (*all && occurrence.is_some())
                                    || occurrence == &Some(0)
                                {
                                    return Err("replace_text needs nonempty old and either occurrence (1-based) or all:true".into());
                                }
                                let old = old.replace("\r\n", "\n");
                                let mut hits = doc
                                    .text
                                    .match_indices(&old)
                                    .map(|(start, s)| (start, start + s.len()))
                                    .collect::<Vec<_>>();
                                if hits.is_empty() {
                                    hits = normalized_matches(&doc.text, &old)?;
                                    if !hits.is_empty() {
                                        notes.push(format!(
                                            "{}: whitespace-normalized match used",
                                            path.display()
                                        ));
                                    }
                                }
                                if hits.is_empty() {
                                    return Err(recovery(
                                        &path,
                                        original,
                                        "old text not found",
                                        &[],
                                    ));
                                }
                                let chosen = if *all {
                                    hits.clone()
                                } else if let Some(n) = occurrence {
                                    vec![*hits.get(n - 1).ok_or_else(|| {
                                        recovery(
                                            &path,
                                            original,
                                            &format!(
                                                "occurrence {n} outside {} candidates",
                                                hits.len()
                                            ),
                                            &candidate_lines(&doc.text, &hits),
                                        )
                                    })?]
                                } else if hits.len() == 1 {
                                    hits.clone()
                                } else {
                                    return Err(recovery(
                                        &path,
                                        original,
                                        "ambiguous text match; select occurrence or all",
                                        &candidate_lines(&doc.text, &hits),
                                    ));
                                };
                                for (start, end) in chosen {
                                    patches.push(Patch {
                                        start,
                                        end,
                                        text: new.replace("\r\n", "\n"),
                                    });
                                }
                            }
                            _ => {
                                return Err(
                                    "file-level and content operations cannot be combined".into()
                                );
                            }
                        }
                    }
                    patches.sort_by_key(|p| (p.start, p.end));
                    for pair in patches.windows(2) {
                        if pair[1].start < pair[0].end || pair[0].start == pair[1].start {
                            return Err("overlapping edits or duplicate insertion points; combine them into one operation".into());
                        }
                    }
                    let mut next = doc.text.clone();
                    for patch in patches.iter().rev() {
                        next.replace_range(patch.start..patch.end, &patch.text);
                    }
                    // Preserve the original EOF convention, including text fallback.
                    if !next.is_empty() {
                        if doc.text.ends_with('\n') && !next.ends_with('\n') {
                            next.push('\n');
                        }
                        if !doc.text.ends_with('\n') {
                            while next.ends_with('\n') {
                                next.pop();
                            }
                        }
                    }
                    if next.contains('\r') || next.contains('\0') {
                        return Err("inserted text contains CR-only line endings or NUL".into());
                    }
                    if doc.encode(&next).len() as u64 > text::MAX_BYTES {
                        return Err("edited file exceeds 32 MiB".into());
                    }
                    after.get_mut(&path).expect("known path").bytes = Some(doc.encode(&next));
                }
            }
        }
        if after
            .values()
            .map(|s| s.bytes.as_ref().map_or(0, Vec::len))
            .sum::<usize>()
            > 64 * 1024 * 1024
        {
            return Err("transaction exceeds 64 MiB output budget".into());
        }
        validate(&before, &after, &args, &mut notes)?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut report = report(&id, &before, &after, args.dry_run, &notes)?;
        if !args.dry_run {
            // Acquire bookkeeping before publication: never report a failed edit
            // after successfully changing bytes because the history mutex failed.
            let mut history = self.history.lock().map_err(|_| "undo history poisoned")?;
            let warnings = publish(store, &before, &after)?;
            report.push_str(&warnings);
            history.push_back(Undo { id, before, after });
            while history.len() > 32
                || history.iter().map(undo_size).sum::<usize>() > 64 * 1024 * 1024
            {
                history.pop_front();
            }
        }
        text::bounded(report, max_tokens)
    }

    fn undo(&self, store: &Arc<HashStore>, id: &str, args: &Args) -> Result<String, String> {
        let record = self.history.lock().map_err(|_| "undo history poisoned")?.iter().find(|u| u.id == id).cloned().ok_or_else(|| "unknown or expired edit_id (history is process-local, max 32 transactions / 64 MiB)".to_owned())?;
        let _guards = record
            .after
            .keys()
            .map(|p| store.lock_edit(p).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let current = record
            .after
            .keys()
            .map(|p| load(p).map(|s| (p.clone(), s)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        for (path, state) in &current {
            if state.bytes != record.after[path].bytes {
                return Err(recovery(
                    path,
                    state,
                    "undo refused: file changed since this edit",
                    &[],
                ));
            }
        }
        let new_id = uuid::Uuid::new_v4().to_string();
        let mut response = report(
            &new_id,
            &current,
            &record.before,
            args.dry_run,
            &[format!("undo_of={id}")],
        )?;
        if !args.dry_run {
            let mut history = self.history.lock().map_err(|_| "undo history poisoned")?;
            response.push_str(&publish(store, &current, &record.before)?);
            history.retain(|r| r.id != id);
            history.push_back(Undo {
                id: new_id,
                before: current,
                after: record.before,
            });
            while history.len() > 32
                || history.iter().map(undo_size).sum::<usize>() > 64 * 1024 * 1024
            {
                history.pop_front();
            }
        }
        text::bounded(response, args.max_tokens.unwrap_or(4096))
    }
}

fn text_for(op: &Operation) -> Option<&str> {
    if let Operation::Replace { text, .. } = op {
        Some(text)
    } else {
        None
    }
}
fn canonical(store: &HashStore, path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("path cannot be empty".into());
    }
    let path = Path::new(path);
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("edit symlink targets explicitly, not symlink paths".into());
    }
    if path.exists() {
        return Ok(store.canonicalize(path));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|e| format!("parent must exist: {e}"))?;
    Ok(parent.join(path.file_name().ok_or("path needs a file name")?))
}
fn load(path: &Path) -> Result<State, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(State {
            bytes: Some(text::read_bytes(path)?),
            permissions: Some(meta.permissions()),
        }),
        Ok(_) => Err(format!("{} is not a regular file", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State {
            bytes: None,
            permissions: None,
        }),
        Err(e) => Err(e.to_string()),
    }
}
fn check_anchor(path: &Path, state: &State, doc: &Document, anchor: &Anchor) -> Result<(), String> {
    if doc
        .anchor(anchor.line)
        .is_some_and(|a| a.hash == anchor.hash)
    {
        return Ok(());
    }
    Err(recovery(path, state, "anchor mismatch", &[anchor.line]))
}
fn recovery(path: &Path, state: &State, reason: &str, candidates: &[usize]) -> String {
    let Some(bytes) = &state.bytes else {
        return format!("{reason}\n{} version=missing", path.display());
    };
    let mut out = format!(
        "{reason}\n¶{} version={}\n",
        path.display(),
        text::version(bytes)
    );
    if let Ok(doc) = Document::decode(bytes) {
        let total = doc.ranges().len();
        let mut selected = BTreeSet::new();
        if candidates.is_empty() {
            selected.extend(1..=total.min(12));
        }
        for line in candidates.iter().take(100) {
            selected.extend(line.saturating_sub(2).max(1)..=line.saturating_add(2).min(total));
        }
        out.push_str(&doc.rows(selected, true, 300));
        if candidates.len() > 100 {
            out.push_str("[overflow: more than 100 candidates; narrow old text]\n");
        }
    }
    text::bounded(out, 4096).unwrap_or_else(|e| e)
}
fn line_text(text: &str, trailing: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut text = text.replace("\r\n", "\n");
    if trailing && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}
fn candidate_lines(text: &str, hits: &[(usize, usize)]) -> Vec<usize> {
    hits.iter()
        .map(|(p, _)| text[..*p].bytes().filter(|b| *b == b'\n').count() + 1)
        .collect()
}
fn normalized_matches(text: &str, old: &str) -> Result<Vec<(usize, usize)>, String> {
    let mut pattern = String::new();
    let mut literal = String::new();
    let mut in_ws = false;
    for ch in old.chars() {
        if ch.is_whitespace() {
            if !literal.is_empty() {
                pattern.push_str(&regex::escape(&literal));
                literal.clear();
            }
            if !in_ws {
                pattern.push_str("\\s+");
                in_ws = true;
            }
        } else {
            literal.push(ch);
            in_ws = false;
        }
    }
    pattern.push_str(&regex::escape(&literal));
    let re = regex::Regex::new(&pattern).map_err(|e| e.to_string())?;
    Ok(re.find_iter(text).map(|m| (m.start(), m.end())).collect())
}
fn warn_indent(old: &str, new: &str, notes: &mut Vec<String>) {
    let style = |s: &str| {
        s.lines().filter(|l| !l.trim().is_empty()).find_map(|l| {
            if l.starts_with('\t') {
                Some('t')
            } else if l.starts_with(' ') {
                Some('s')
            } else {
                None
            }
        })
    };
    if let (Some(a), Some(b)) = (style(old), style(new))
        && a != b
    {
        notes.push(
            "warning: inserted indentation differs from surrounding code (tabs/spaces)".into(),
        );
    }
}
fn undo_size(u: &Undo) -> usize {
    u.before
        .values()
        .chain(u.after.values())
        .map(|s| s.bytes.as_ref().map_or(0, Vec::len))
        .sum()
}

fn validate(
    before: &BTreeMap<PathBuf, State>,
    after: &BTreeMap<PathBuf, State>,
    args: &Args,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    if !args.validate {
        notes.push("syntax validation disabled".into());
        return Ok(());
    }
    for (path, state) in after {
        if state.bytes == before[path].bytes {
            continue;
        }
        if let Some(bytes) = &state.bytes {
            let doc = Document::decode(bytes)?;
            match syntax::errors(path, &doc.text)? {
                None => notes.push(format!(
                    "{}: syntax validation unsupported for this extension",
                    path.display()
                )),
                Some(errors) if !errors.is_empty() => {
                    let message = format!(
                        "{}: syntax errors in proposed content:\n{}",
                        path.display(),
                        errors.join("\n")
                    );
                    if args.auto_rollback {
                        return Err(format!(
                            "{message}\nNo files changed (validation ran before commit). Set auto_rollback:false to apply and report."
                        ));
                    }
                    notes.push(message);
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

// Prepare every file before publishing any. Roll back observed failures; report
// rollback failures explicitly. No claim of crash-atomic cross-file visibility.
fn publish(
    store: &HashStore,
    before: &BTreeMap<PathBuf, State>,
    after: &BTreeMap<PathBuf, State>,
) -> Result<String, String> {
    publish_checked(store, before, after, |_| Ok(()))
}
fn publish_checked(
    store: &HashStore,
    before: &BTreeMap<PathBuf, State>,
    after: &BTreeMap<PathBuf, State>,
    check: impl Fn(&Path) -> Result<(), String>,
) -> Result<String, String> {
    let mut stages = BTreeMap::new();
    for (path, state) in after {
        if state.bytes == before[path].bytes {
            continue;
        }
        if let Some(bytes) = &state.bytes {
            stages.insert(path.clone(), stage(path, bytes, state.permissions.clone())?);
        }
    }
    for (path, state) in before {
        if load(path)?.bytes != state.bytes {
            return Err(format!(
                "{} changed during validation; no files changed",
                path.display()
            ));
        }
    }
    let mut applied: Vec<PathBuf> = Vec::new();
    for (path, state) in after {
        if state.bytes == before[path].bytes {
            continue;
        }
        let result = check(path).and_then(|()| {
            if state.bytes.is_some() {
                stages
                    .remove(path)
                    .expect("staged")
                    .persist(path)
                    .map(|_| ())
                    .map_err(|e| e.error.to_string())
            } else {
                fs::remove_file(path).map_err(|e| e.to_string())
            }
        });
        if let Err(error) = result {
            let mut failures = Vec::new();
            for p in applied.iter().rev() {
                let previous = &before[p];
                let restored = if let Some(bytes) = &previous.bytes {
                    store
                        .atomic_write(
                            p,
                            p.parent().expect("parent"),
                            bytes,
                            previous.permissions.clone(),
                            "edit rollback",
                        )
                        .map_err(|e| e.to_string())
                } else {
                    fs::remove_file(p).map_err(|e| e.to_string())
                };
                if let Err(e) = restored {
                    failures.push(format!("{}: {e}", p.display()));
                }
            }
            return Err(format!(
                "commit failed at {}: {error}; {}",
                path.display(),
                if failures.is_empty() {
                    "all published changes rolled back".into()
                } else {
                    format!("ROLLBACK INCOMPLETE: {}", failures.join("; "))
                }
            ));
        }
        applied.push(path.clone());
    }
    let mut warnings = Vec::new();
    for parent in after
        .keys()
        .filter_map(|p| p.parent())
        .collect::<BTreeSet<_>>()
    {
        if let Err(e) = fs::File::open(parent).and_then(|f| f.sync_all()) {
            warnings.push(format!("{}: {e}", parent.display()));
        }
    }
    if warnings.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(
            "warning: edit committed but directory sync failed: {}\n",
            warnings.join("; ")
        ))
    }
}
fn stage(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<tempfile::NamedTempFile, String> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().ok_or("missing parent")?)
        .map_err(|e| e.to_string())?;
    if let Some(p) = permissions {
        file.as_file()
            .set_permissions(p)
            .map_err(|e| e.to_string())?;
    }
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    Ok(file)
}
fn report(
    id: &str,
    before: &BTreeMap<PathBuf, State>,
    after: &BTreeMap<PathBuf, State>,
    dry_run: bool,
    notes: &[String],
) -> Result<String, String> {
    let mut out = format!(
        "{} edit_id={id}\n",
        if dry_run {
            "DRY_RUN (no files changed)"
        } else {
            "OK"
        }
    );
    for note in notes {
        out.push_str(&format!("{note}\n"));
    }
    for (path, state) in after {
        if state.bytes == before[path].bytes {
            continue;
        }
        let old = before[path]
            .bytes
            .as_ref()
            .and_then(|b| Document::decode(b).ok())
            .map(|d| d.text)
            .unwrap_or_default();
        let new = state
            .bytes
            .as_ref()
            .map(|b| Document::decode(b))
            .transpose()?;
        let content = new.as_ref().map_or("", |d| d.text.as_str());
        let version = state
            .bytes
            .as_deref()
            .map(text::version)
            .unwrap_or_else(|| "missing".into());
        out.push_str(&format!("¶{} version={version}\n", path.display()));
        let diff = similar::TextDiff::configure()
            .timeout(std::time::Duration::from_millis(250))
            .diff_lines(old.as_str(), content);
        // Never build an unbounded diff before enforcing the response budget.
        let mut rendered = LimitedBuffer {
            bytes: Vec::new(),
            limit: 128 * 1024,
        };
        if diff
            .unified_diff()
            .context_radius(2)
            .header("before", "after")
            .to_writer(&mut rendered)
            .is_ok()
        {
            out.push_str(&String::from_utf8_lossy(&rendered.bytes));
        } else {
            out.push_str("[overflow: diff exceeds 128 KiB; fresh anchors below]\n");
        }
        if let Some(doc) = &new {
            let mut selected = BTreeSet::new();
            let total = doc.ranges().len();
            for op in diff.ops() {
                if op.tag() != similar::DiffTag::Equal {
                    let range = op.new_range();
                    selected.extend(
                        range.start.saturating_sub(2).max(1)
                            ..=range.end.saturating_add(2).min(total),
                    );
                }
            }
            if selected.is_empty() {
                selected.extend(1..=total.min(5));
            }
            out.push_str(&doc.rows(selected, true, 300));
        }
    }
    Ok(out)
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for LimitedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(buf.len()) > self.limit {
            return Err(std::io::Error::other("diff output limit"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    #[test]
    fn publish_failure_restores_already_published_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, "old a").unwrap();
        fs::write(&b, "old b").unwrap();
        let before = [a.clone(), b.clone()]
            .into_iter()
            .map(|p| {
                let s = load(&p).unwrap();
                (p, s)
            })
            .collect::<BTreeMap<_, _>>();
        let mut after = before.clone();
        after.get_mut(&a).unwrap().bytes = Some(b"new a".to_vec());
        after.get_mut(&b).unwrap().bytes = Some(b"new b".to_vec());
        let err = publish_checked(&HashStore::new(), &before, &after, |p| {
            if p == b {
                Err("injected second publication failure".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(err.contains("all published changes rolled back"));
        assert_eq!(fs::read(&a).unwrap(), b"old a");
        assert_eq!(fs::read(&b).unwrap(), b"old b");
    }
}
