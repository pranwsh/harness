use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use harness_contracts::{
    CH_SESSION_APPEND_REQUESTED, CH_SESSION_ENTRY_APPENDED, CH_SESSION_TURN_CLOSE_REQUESTED,
    CH_SESSION_TURN_CLOSED, CH_SESSION_TURN_OPEN_REQUESTED, CH_SESSION_TURN_OPENED,
    CH_TOOL_EXECUTED, ConfigHandle, Entry, KEY_CONFIG, KEY_SESSION_CATALOG, KEY_SESSION_STORE,
    KEY_SESSIONS, Role, SessionAppendRequested, SessionCatalogApi, SessionCatalogHandle,
    SessionEntryAppended, SessionId, SessionStoreApi, SessionStoreHandle, SessionSummary,
    SessionTurnCloseRequested, SessionTurnClosed, SessionTurnOpenRequested, SessionTurnOpened,
    ToolExecuted,
};
use harness_core::{Context, Result};
use serde::{Deserialize, Serialize};

/// Journal format version, written into every file header.
const JOURNAL_VERSION: u32 = 1;

/// First file line of every `sessions/<id>.jsonl` journal.
#[derive(Debug, Serialize, Deserialize)]
struct JournalHeader {
    v: u32,
    id: SessionId,
    created_at: u64,
}

/// One journal line per appended entry. Borrowed on write (no extra clone
/// of potentially large tool outputs), owned on read.
#[derive(Debug, Serialize)]
struct JournalRecord<'a> {
    turn: u64,
    ts: u64,
    entry: &'a Entry,
}

#[derive(Debug, Deserialize)]
struct JournalRecordOwned {
    turn: u64,
    ts: u64,
    entry: Entry,
}

/// Display title budget, in characters.
const MAX_TITLE_CHARS: usize = 60;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// First user message, flattened to one line and truncated, for picker
/// display. Never empty-panics; blank input yields a blank title.
fn title_of(content: &str) -> String {
    let one_line = content.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(MAX_TITLE_CHARS) {
        Some((i, _)) => format!("{}…", &one_line[..i]),
        None => one_line,
    }
}

/// Filename-safe session id: ids are self-generated uuids, but ids also
/// arrive from outside (loop callers), so anything outside
/// `[A-Za-z0-9_-]` maps to `_` and the journal can never escape its
/// directory. The authoritative id always comes from the file header (or
/// the filename stem for headerless files), never the reverse.
fn sanitize_filename(id: &str) -> String {
    let mut out = String::with_capacity(id.len().max(1));
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

/// Monotonic turn/entry bookkeeping for one session.
#[derive(Debug)]
struct Session {
    turn: u64,
    entries: Vec<Entry>,
    title: String,
    created_at: u64,
    updated_at: u64,
}

impl Default for Session {
    fn default() -> Self {
        let now = now_secs();
        Session {
            turn: 0,
            entries: Vec::new(),
            title: String::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

fn summary_of(id: &str, session: &Session) -> SessionSummary {
    SessionSummary {
        id: id.to_owned(),
        title: session.title.clone(),
        created_at: session.created_at,
        updated_at: session.updated_at,
        turns: session.turn,
        entries: session.entries.len(),
    }
}

/// Best-effort parse of one journal file. Returns `None` when the file is
/// missing, unreadable, or holds no header and no valid records. Corrupt
/// lines are skipped; a headerless file still loads via its filename stem
/// (appends self-heal it — see `journal_append`), with `created_at`
/// falling back to the newest record timestamp.
fn read_journal(path: &Path) -> Option<(SessionId, Session)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut id: Option<SessionId> = None;
    let mut session = Session {
        turn: 0,
        entries: Vec::new(),
        title: String::new(),
        // Zeroed on purpose (not `Session::default()`): `updated_at` is
        // folded with `max(record.ts)`, so seeding `now` here would clamp
        // every reloaded journal to startup time and all past sessions
        // would look freshly active. `created_at` falls back to
        // the newest record ts below for headerless journals.
        created_at: 0,
        updated_at: 0,
    };
    let mut records = 0u32;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A record line can never parse as a header (missing `v`/`id`/
        // `created_at` is a hard error without `#[serde(default)]`), so
        // trying the header first is unambiguous.
        if let Ok(header) = serde_json::from_str::<JournalHeader>(line) {
            if header.v == JOURNAL_VERSION && id.is_none() {
                session.created_at = header.created_at;
                id = Some(header.id);
            }
            continue;
        }
        let Ok(record) = serde_json::from_str::<JournalRecordOwned>(line) else {
            continue;
        };
        session.turn = session.turn.max(record.turn);
        session.updated_at = session.updated_at.max(record.ts);
        if session.title.is_empty() && record.entry.role == Role::User {
            session.title = title_of(&record.entry.content);
        }
        session.entries.push(record.entry);
        records += 1;
    }
    if records == 0 {
        return None;
    }
    let id = match id {
        Some(id) => id,
        None => path.file_stem()?.to_str()?.to_owned(),
    };
    if session.created_at == 0 {
        session.created_at = session.updated_at;
    }
    Some((id, session))
}

/// Resolves the journal directory: explicit config override, else
/// `$XDG_DATA_HOME/harness/sessions`, else `$HOME/.local/share/harness/
/// sessions`. `None` when nothing usable is configured (no `HOME`), in
/// which case the caller degrades to in-memory.
fn resolve_dir(configured: Option<String>) -> Option<PathBuf> {
    if let Some(dir) = configured.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("harness/sessions"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".local/share/harness/sessions"))
}

/// Decides the journal directory. `None` means in-memory only: persistence
/// disabled in config, no config service at all (tests/embedded contexts
/// must never touch `$HOME`), an embedded config with no explicit `dir`
/// (embedded configs don't invent disk state), or an unresolvable home /
/// uncreatable directory (degrades with a warning, never a hard error).
///
/// Pure except for the `create_dir_all`, so the decision itself is
/// unit-testable.
fn setup_persistence(
    config: Option<harness_contracts::SessionConfig>,
    anchored: bool,
) -> Option<PathBuf> {
    let config = config.filter(|c| c.enabled)?;
    let dir = match config.dir.filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None if anchored => resolve_dir(None)?,
        None => return None,
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "session: cannot create {}: {e}; continuing in-memory",
            dir.display()
        );
        return None;
    }
    Some(dir)
}

/// Append-only conversation log, journaled to disk when persistence is
/// enabled.
///
/// This plugin is the sole persistence owner. All mutations arrive via its
/// own bus handlers — turn open via `session.turn_open_requested` waterfall
/// (returns the assigned turn), appends via `session.append_requested`
/// sync listener, turn close via `session.turn_close_requested` sync
/// listener, and tool results via `tool.executed` sync listener — all
/// running inline on the emitting thread so the mutation is guaranteed once
/// the emitter resumes. Direct `begin_turn`/`end_turn`/`append` calls exist
/// for tests and bootstrap but loop producers must use the bus.
///
/// With persistence enabled, every append journals one JSONL line to
/// `sessions/<id>.jsonl` (header lazily written on first use) and every
/// turn close fsyncs, so a crash loses at most the in-flight turn. Reads
/// (`history`/`len`/`begin_turn`) lazily hydrate unloaded sessions from
/// disk, restoring the turn counter to the max recorded turn. The picker
/// catalog (`list`) is served from an in-memory index updated on every
/// mutation and rebuilt by a directory scan at startup — never a per-call
/// disk walk.
pub struct SessionLog {
    ctx: Context,
    sessions: Mutex<HashMap<SessionId, Session>>,
    persist: Option<PathBuf>,
    index: Mutex<HashMap<SessionId, SessionSummary>>,
}

impl SessionLog {
    pub fn new(ctx: Context) -> Self {
        SessionLog {
            ctx,
            sessions: Mutex::new(HashMap::new()),
            persist: None,
            index: Mutex::new(HashMap::new()),
        }
    }

    /// Disk-backed log over `dir`: scans existing journals into the catalog
    /// index (entries stay on disk until first touched). `dir` must exist.
    pub fn with_persistence(ctx: Context, dir: PathBuf) -> Self {
        let log = SessionLog {
            ctx,
            sessions: Mutex::new(HashMap::new()),
            persist: Some(dir.clone()),
            index: Mutex::new(HashMap::new()),
        };
        log.scan(&dir);
        log
    }

    /// Fresh session id for process start. Compact uuid hex: filename-safe
    /// and short enough for picker prefixes.
    pub fn fresh_id() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    /// Opens a new turn, incrementing the per-session turn counter.
    /// Emits `session.turn_opened`.
    pub fn begin_turn(&self, session_id: &str) -> u64 {
        self.ensure_loaded(session_id);
        let turn = {
            let mut sessions = self.lock();
            let session = sessions.entry(session_id.to_owned()).or_default();
            session.turn += 1;
            session.updated_at = now_secs();
            let turn = session.turn;
            self.lock_index()
                .insert(session_id.to_owned(), summary_of(session_id, session));
            turn
        };
        if let Err(e) = self.ctx.emit_key(
            CH_SESSION_TURN_OPENED,
            SessionTurnOpened {
                session_id: session_id.to_owned(),
                turn,
            },
        ) {
            eprintln!("session: emit {} failed: {e}", CH_SESSION_TURN_OPENED);
        }
        turn
    }

    /// Closes the current turn: fsyncs the journal, then emits
    /// `session.turn_closed`.
    pub fn end_turn(&self, session_id: &str, turn: u64) {
        // Missing file is fine (a turn with no appends journals nothing);
        // otherwise flush OS buffers so the closed turn is crash-safe.
        if let Some(path) = self.journal_path(session_id)
            && let Ok(file) = std::fs::OpenOptions::new().append(true).open(&path)
            && let Err(e) = file.sync_all()
        {
            eprintln!("session: fsync {} failed: {e}", path.display());
        }
        if let Err(e) = self.ctx.emit_key(
            CH_SESSION_TURN_CLOSED,
            SessionTurnClosed {
                session_id: session_id.to_owned(),
                turn,
            },
        ) {
            eprintln!("session: emit {} failed: {e}", CH_SESSION_TURN_CLOSED);
        }
    }

    /// Appends an entry: in-memory, index, journal line, then emits
    /// `session.entry_appended`. File I/O runs after the locks are
    /// released so a slow disk never blocks other sessions.
    pub fn append(&self, session_id: &str, turn: u64, entry: Entry) {
        self.ensure_loaded(session_id);
        let ts = now_secs();
        let created_at = {
            let mut sessions = self.lock();
            let session = sessions.entry(session_id.to_owned()).or_default();
            session.updated_at = ts;
            if session.title.is_empty() && entry.role == Role::User {
                session.title = title_of(&entry.content);
            }
            session.entries.push(entry.clone());
            let created_at = session.created_at;
            self.lock_index()
                .insert(session_id.to_owned(), summary_of(session_id, session));
            created_at
        };
        self.journal_append(session_id, turn, ts, created_at, &entry);
        if let Err(e) = self.ctx.emit_key(
            CH_SESSION_ENTRY_APPENDED,
            SessionEntryAppended {
                session_id: session_id.to_owned(),
                turn,
                entry,
            },
        ) {
            eprintln!("session: emit {} failed: {e}", CH_SESSION_ENTRY_APPENDED);
        }
    }

    /// Snapshot of a session's entries, hydrating from disk on first touch.
    pub fn history(&self, session_id: &str) -> Vec<Entry> {
        self.ensure_loaded(session_id);
        self.lock()
            .get(session_id)
            .map(|s| s.entries.clone())
            .unwrap_or_default()
    }

    /// Number of entries recorded for a session.
    pub fn len(&self, session_id: &str) -> usize {
        self.history(session_id).len()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, Session>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_index(&self) -> MutexGuard<'_, HashMap<SessionId, SessionSummary>> {
        self.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn journal_path(&self, session_id: &str) -> Option<PathBuf> {
        self.persist
            .as_ref()
            .map(|dir| dir.join(format!("{}.jsonl", sanitize_filename(session_id))))
    }

    /// Hydrates one session from its journal unless already resident. Fast
    /// path is a single lock + map probe; the file is read at most once
    /// per session per process. Lock order is always sessions-then-index,
    /// matching the mutation paths.
    fn ensure_loaded(&self, session_id: &str) {
        if self.lock().contains_key(session_id) || self.persist.is_none() {
            return;
        }
        let Some(path) = self.journal_path(session_id) else {
            return;
        };
        let Some((id, session)) = read_journal(&path) else {
            return;
        };
        if id != session_id {
            return;
        }
        let summary = summary_of(&id, &session);
        // `or_insert`: a concurrent touch may have beaten us here; last
        // writer wins on the index either way, sessions stay single-turn
        // so content cannot divergently interleave.
        self.lock().entry(id.clone()).or_insert(session);
        self.lock_index().insert(id, summary);
    }

    /// Startup scan: catalog summaries for every readable journal. Entries
    /// stay on disk until first touched.
    fn scan(&self, dir: &Path) {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some((id, session)) = read_journal(&path) {
                self.lock_index()
                    .insert(id.clone(), summary_of(&id, &session));
            }
        }
    }

    /// Appends one journal line (writing the header first when the file is
    /// new or empty, which also self-heals headerless files). No-op without
    /// persistence. Failures log and continue in-memory — a full disk must
    /// never break the live turn.
    fn journal_append(&self, session_id: &str, turn: u64, ts: u64, created_at: u64, entry: &Entry) {
        let Some(path) = self.journal_path(session_id) else {
            return;
        };
        let fresh = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) == 0;
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) => {
                eprintln!("session: journal {} failed: {e}", path.display());
                return;
            }
        };
        if fresh {
            let header = JournalHeader {
                v: JOURNAL_VERSION,
                id: session_id.to_owned(),
                created_at,
            };
            match serde_json::to_string(&header) {
                Ok(line) => {
                    if writeln!(file, "{line}").is_err() {
                        eprintln!("session: journal {} failed", path.display());
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("session: journal header encode failed: {e}");
                    return;
                }
            }
        }
        let record = JournalRecord { turn, ts, entry };
        match serde_json::to_string(&record) {
            Ok(line) => {
                if writeln!(file, "{line}").is_err() {
                    eprintln!("session: journal {} failed", path.display());
                }
            }
            Err(e) => eprintln!("session: journal record encode failed: {e}"),
        }
    }
}

impl SessionStoreApi for SessionLog {
    fn begin_turn(&self, session_id: &str) -> u64 {
        SessionLog::begin_turn(self, session_id)
    }

    fn end_turn(&self, session_id: &str, turn: u64) {
        SessionLog::end_turn(self, session_id, turn)
    }

    fn history(&self, session_id: &str) -> Vec<Entry> {
        SessionLog::history(self, session_id)
    }
}

impl SessionCatalogApi for SessionLog {
    fn list(&self) -> Vec<SessionSummary> {
        let mut summaries: Vec<SessionSummary> = self.lock_index().values().cloned().collect();
        // Newest activity first; id tie-break keeps the order deterministic
        // within one-second timestamp granularity.
        summaries.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        summaries
    }
}

pub struct SessionPlugin;

impl harness_core::Plugin for SessionPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("session")
            .provides(KEY_SESSIONS)
            .provides(KEY_SESSION_STORE)
            .provides(KEY_SESSION_CATALOG)
            .emits::<SessionTurnOpened>(CH_SESSION_TURN_OPENED)
            .emits::<SessionEntryAppended>(CH_SESSION_ENTRY_APPENDED)
            .emits::<SessionTurnClosed>(CH_SESSION_TURN_CLOSED)
            .waterfalls::<SessionTurnOpenRequested>(CH_SESSION_TURN_OPEN_REQUESTED)
            .listens::<SessionTurnCloseRequested>(CH_SESSION_TURN_CLOSE_REQUESTED)
            .listens::<SessionAppendRequested>(CH_SESSION_APPEND_REQUESTED)
            .listens::<ToolExecuted>(CH_TOOL_EXECUTED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        // Optional on purpose (no `injects` declaration, so parking behavior
        // is unchanged): without a config service (tests/embedded) the log
        // stays in-memory and never touches the filesystem. With a config
        // but no backing file (embedded TOML), only an explicit `[session]
        // dir` enables disk writes — default persistence anchors to
        // file-backed configs so test/library use stays hermetic.
        let (config, anchored) = match ctx.try_inject_key::<ConfigHandle>(KEY_CONFIG) {
            Some(handle) => {
                let anchored = handle.config_path().is_some();
                (Some(handle.get().session), anchored)
            }
            None => (None, false),
        };
        let log = Arc::new(match setup_persistence(config, anchored) {
            Some(dir) => SessionLog::with_persistence(ctx.clone(), dir),
            None => SessionLog::new(ctx.clone()),
        });
        ctx.provide_key(KEY_SESSIONS, log.clone());
        ctx.provide_key(
            KEY_SESSION_STORE,
            Arc::new(SessionStoreHandle(log.clone() as Arc<dyn SessionStoreApi>)),
        );
        ctx.provide_key(
            KEY_SESSION_CATALOG,
            Arc::new(SessionCatalogHandle(
                log.clone() as Arc<dyn SessionCatalogApi>
            )),
        );

        // Canonical bus paths, all inline: turn open via waterfall
        // (returns assigned turn, fail-closed when no handler), appends via
        // `session.append_requested`, turn close via
        // `session.turn_close_requested`, and tool results via
        // `tool.executed`. Sync/waterfall handlers run on the emitting
        // thread, so the mutation (including its journal line) is
        // guaranteed once the emitter resumes.
        {
            let log = log.clone();
            ctx.on_waterfall_key::<SessionTurnOpenRequested, _, _>(
                CH_SESSION_TURN_OPEN_REQUESTED,
                move |req| {
                    let log = log.clone();
                    async move {
                        let mut next = (*req).clone();
                        next.turn = log.begin_turn(&req.session_id);
                        next
                    }
                },
            )?;
        }
        {
            let log = log.clone();
            ctx.on_sync_key::<SessionTurnCloseRequested, _>(
                CH_SESSION_TURN_CLOSE_REQUESTED,
                move |ev| {
                    log.end_turn(&ev.session_id, ev.turn);
                },
            )?;
        }
        {
            let log = log.clone();
            ctx.on_sync_key::<SessionAppendRequested, _>(CH_SESSION_APPEND_REQUESTED, move |ev| {
                log.append(&ev.session_id, ev.turn, ev.entry.clone());
            })?;
        }
        ctx.on_sync_key::<ToolExecuted, _>(CH_TOOL_EXECUTED, move |ev| {
            let entry = match &ev.result {
                Ok(output) => Entry::tool(&ev.call.id, output),
                Err(err) => Entry::tool(&ev.call.id, format!("error: {err}")),
            };
            log.append(&ev.session_id, ev.turn, entry);
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{Role, SessionConfig, ToolCall, ToolError};

    fn tool_event(
        session: &str,
        turn: u64,
        call_id: &str,
        result: Result<String, ToolError>,
    ) -> ToolExecuted {
        ToolExecuted {
            agent_id: "a".into(),
            session_id: session.into(),
            turn,
            call: ToolCall {
                id: call_id.into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            result,
        }
    }

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "harness-session-test-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn begin_turn_increments_and_emits() {
        let ctx = Context::root();
        ctx.load(SessionPlugin).unwrap();
        let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();

        let opened = Arc::new(std::sync::atomic::AtomicU64::new(0));
        ctx.on_sync_key::<SessionTurnOpened, _>(CH_SESSION_TURN_OPENED, {
            let n = opened.clone();
            move |_| {
                n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .unwrap();

        assert_eq!(log.begin_turn("s"), 1);
        assert_eq!(log.begin_turn("s"), 2);
        assert_eq!(log.begin_turn("other"), 1);
        assert_eq!(opened.load(std::sync::atomic::Ordering::Relaxed), 3);
    }

    #[test]
    fn tool_results_are_appended_inline() {
        let ctx = Context::root();
        ctx.load(SessionPlugin).unwrap();
        let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();

        let turn = log.begin_turn("s");
        let ev = tool_event("s", turn, "t1", Ok("file body".into()));
        let _ = ctx.emit_key(CH_TOOL_EXECUTED, ev).unwrap();

        let history = log.history("s");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::Tool);
        assert_eq!(history[0].call_id.as_deref(), Some("t1"));
        assert_eq!(history[0].content, "file body");
    }

    #[test]
    fn tool_errors_are_recorded_as_tool_entries() {
        let ctx = Context::root();
        ctx.load(SessionPlugin).unwrap();
        let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();

        let turn = log.begin_turn("s");
        let ev = tool_event(
            "s",
            turn,
            "t9",
            Err(ToolError {
                tool: "read_file".into(),
                message: "boom".into(),
            }),
        );
        let _ = ctx.emit_key(CH_TOOL_EXECUTED, ev).unwrap();

        let history = log.history("s");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::Tool);
        assert!(history[0].content.contains("boom"));
    }

    #[test]
    fn append_requests_are_appended_inline() {
        use harness_contracts::{CH_SESSION_APPEND_REQUESTED, Message, SessionAppendRequested};

        let ctx = Context::root();
        ctx.load(SessionPlugin).unwrap();
        let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();

        let turn = log.begin_turn("s");
        let ev = SessionAppendRequested {
            session_id: "s".into(),
            turn,
            entry: Entry::from_message(&Message::user("hello")),
        };
        let _ = ctx.emit_key(CH_SESSION_APPEND_REQUESTED, ev).unwrap();

        let history = log.history("s");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, "hello");
    }

    #[test]
    fn history_of_unknown_session_is_empty() {
        let ctx = Context::root();
        ctx.load(SessionPlugin).unwrap();
        let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();
        assert!(log.history("nope").is_empty());
        assert_eq!(log.len("nope"), 0);
    }

    #[test]
    fn journal_round_trip_restores_history_turns_and_catalog() {
        use harness_contracts::Message;

        let dir = unique_tmp_dir("roundtrip");
        let ctx = Context::root();
        let log = SessionLog::with_persistence(ctx.clone(), dir.clone());

        let turn = log.begin_turn("s1");
        assert_eq!(turn, 1);
        log.append(
            "s1",
            turn,
            Entry::from_message(&Message::user("hello world")),
        );
        log.append(
            "s1",
            turn,
            Entry::from_message(&Message::assistant("hi there")),
        );
        log.append("s1", turn, Entry::tool("t1", "file body"));
        log.end_turn("s1", turn);

        // One journal file, header + three records.
        let journal = dir.join("s1.jsonl");
        let lines: Vec<String> = std::fs::read_to_string(&journal)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 4, "{lines:?}");

        // A fresh log over the same dir recovers everything lazily.
        let ctx2 = Context::root();
        let reloaded = SessionLog::with_persistence(ctx2, dir);
        let history = reloaded.history("s1");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, "hello world");
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[2].call_id.as_deref(), Some("t1"));
        // Turn counter resumes past the recorded turn.
        assert_eq!(reloaded.begin_turn("s1"), 2);

        let catalog = reloaded.list();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "s1");
        assert_eq!(catalog[0].title, "hello world");
        assert_eq!(catalog[0].turns, 2);
        assert_eq!(catalog[0].entries, 3);
    }

    #[test]
    fn sparse_turn_numbers_restore_to_max_recorded_turn() {
        use harness_contracts::Message;

        let dir = unique_tmp_dir("sparse");
        let log = SessionLog::with_persistence(Context::root(), dir.clone());
        log.append("s", 5, Entry::from_message(&Message::user("late")));
        log.end_turn("s", 5);

        let reloaded = SessionLog::with_persistence(Context::root(), dir);
        assert_eq!(reloaded.begin_turn("s"), 6);
    }

    #[test]
    fn corrupt_journal_lines_are_skipped() {
        let dir = unique_tmp_dir("corrupt");
        std::fs::write(
            dir.join("s.jsonl"),
            concat!(
                "not json at all\n",
                "{\"v\":1,\"id\":\"s\",\"created_at\":100}\n",
                "{\"turn\":1,\n",
                "{\"turn\":1,\"ts\":200,\"entry\":{\"role\":\"user\",\"content\":\"kept\"}}\n",
            ),
        )
        .unwrap();

        let log = SessionLog::with_persistence(Context::root(), dir);
        let history = log.history("s");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "kept");
        let catalog = log.list();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].created_at, 100);
    }

    #[test]
    fn headerless_journal_loads_via_filename_stem() {
        let dir = unique_tmp_dir("headerless");
        // No header line: records alone must still load.
        std::fs::write(
            dir.join("abc.jsonl"),
            "{\"turn\":2,\"ts\":300,\"entry\":{\"role\":\"assistant\",\"content\":\"x\"}}\n",
        )
        .unwrap();

        let log = SessionLog::with_persistence(Context::root(), dir);
        assert_eq!(log.history("abc").len(), 1);
        assert_eq!(log.begin_turn("abc"), 3);
    }

    #[test]
    fn scan_lists_sessions_newest_first() {
        let dir = unique_tmp_dir("scan");
        // Hand-written journals with explicit timestamps: deterministic
        // order without sleeping for a second boundary.
        std::fs::write(
            dir.join("old.jsonl"),
            "{\"v\":1,\"id\":\"old\",\"created_at\":100}\n{\"turn\":1,\"ts\":100,\"entry\":{\"role\":\"user\",\"content\":\"first\"}}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("new.jsonl"),
            "{\"v\":1,\"id\":\"new\",\"created_at\":200}\n{\"turn\":3,\"ts\":300,\"entry\":{\"role\":\"user\",\"content\":\"second\"}}\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "not a journal").unwrap();

        let log = SessionLog::with_persistence(Context::root(), dir);
        let catalog = log.list();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].id, "new");
        assert_eq!(catalog[0].turns, 3);
        assert_eq!(catalog[1].id, "old");
        // Non-journal files are ignored, and nothing was hydrated yet.
        assert!(log.lock().is_empty());
    }

    #[test]
    fn reloaded_catalog_preserves_record_timestamps() {
        let dir = unique_tmp_dir("timestamps");
        // Old timestamps: a reload must keep them, not clamp to startup
        // time (which made every past session read as freshly active).
        std::fs::write(
            dir.join("old.jsonl"),
            "{\"v\":1,\"id\":\"old\",\"created_at\":100}\n{\"turn\":1,\"ts\":200,\"entry\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();

        let log = SessionLog::with_persistence(Context::root(), dir);
        let catalog = log.list();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].updated_at, 200);
        assert_eq!(catalog[0].created_at, 100);
        // Lazy hydration preserves them too, not just the startup scan.
        assert_eq!(log.history("old").len(), 1);
        let catalog = log.list();
        assert_eq!(catalog[0].updated_at, 200);
    }

    #[test]
    fn filenames_never_escape_the_journal_dir() {
        let dir = unique_tmp_dir("escape");
        let log = SessionLog::with_persistence(Context::root(), dir.clone());
        log.append(
            "../../evil",
            1,
            Entry::from_message(&harness_contracts::Message::user("x")),
        );
        // Sanitized into a flat file inside the dir; nothing outside it.
        assert!(dir.join("______evil.jsonl").exists());
        assert!(!dir.join("../../evil.jsonl").exists());
        assert!(std::fs::read_dir(dir).unwrap().count() == 1);
    }

    #[test]
    fn setup_persistence_respects_config() {
        // No config (tests/embedded): in-memory, never touches the disk.
        assert!(setup_persistence(None, true).is_none());
        assert!(setup_persistence(None, false).is_none());
        // Explicitly disabled: override dir is ignored.
        assert!(
            setup_persistence(
                Some(SessionConfig {
                    enabled: false,
                    dir: Some("/tmp/should-never-be-created-by-this-test".into()),
                }),
                true
            )
            .is_none()
        );
        assert!(!std::path::Path::new("/tmp/should-never-be-created-by-this-test").exists());
        // Embedded config without an explicit dir: in-memory, so test and
        // library use stays hermetic and never sprays `$HOME`. (The
        // anchored file-backed default is intentionally not unit-tested:
        // it would create the real user dir.)
        assert!(
            setup_persistence(
                Some(SessionConfig {
                    enabled: true,
                    dir: None
                }),
                false
            )
            .is_none()
        );
        // Explicit dir is honored even for embedded configs (direct intent).
        let dir = unique_tmp_dir("enabled").join("sub");
        let resolved = setup_persistence(
            Some(SessionConfig {
                enabled: true,
                dir: Some(dir.to_str().unwrap().to_owned()),
            }),
            false,
        );
        assert_eq!(resolved.as_deref(), Some(dir.as_path()));
        assert!(dir.is_dir());
    }

    #[test]
    fn disabled_plugin_writes_no_files() {
        use harness_config::ConfigPlugin;

        let dir = unique_tmp_dir("disabled-plugin");
        let raw = format!(
            "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"a\"\n\n[session]\nenabled = false\ndir = \"{}\"\n",
            dir.join("journals").display()
        );
        let ctx = Context::root();
        ctx.load(ConfigPlugin::from_toml(&raw).unwrap()).unwrap();
        ctx.load(SessionPlugin).unwrap();

        let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();
        let turn = log.begin_turn("s");
        log.append(
            "s",
            turn,
            Entry::from_message(&harness_contracts::Message::user("hi")),
        );
        log.end_turn("s", turn);

        // In-memory behavior intact, disk untouched.
        assert_eq!(log.history("s").len(), 1);
        assert_eq!(log.list().len(), 1);
        assert!(!dir.join("journals").exists());
    }

    #[test]
    fn catalog_handle_lists_through_di() {
        use harness_contracts::KEY_SESSION_CATALOG;

        let ctx = Context::root();
        ctx.load(SessionPlugin).unwrap();
        let catalog: Arc<SessionCatalogHandle> = ctx.inject_key(KEY_SESSION_CATALOG).unwrap();
        assert!(catalog.list().is_empty());
    }

    #[test]
    fn fresh_ids_are_unique_and_filename_safe() {
        let a = SessionLog::fresh_id();
        let b = SessionLog::fresh_id();
        assert_ne!(a, b);
        for id in [a, b] {
            assert_eq!(sanitize_filename(&id), id);
            assert!(!id.is_empty());
        }
    }

    #[test]
    fn titles_truncate_to_one_line() {
        assert_eq!(title_of("hello"), "hello");
        assert_eq!(title_of("a\nb  c"), "a b c");
        let long = "x".repeat(100);
        let title = title_of(&long);
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
    }
}
