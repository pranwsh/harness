use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use harness_contracts::{
    CH_SESSION_ENTRY_APPENDED, CH_SESSION_TURN_CLOSED, CH_SESSION_TURN_OPENED, CH_TOOL_EXECUTED,
    Entry, KEY_SESSIONS, SessionEntryAppended, SessionId, SessionTurnClosed, SessionTurnOpened,
    ToolExecuted,
};
use harness_core::{Context, Result};

/// Monotonic turn/entry bookkeeping for one session.
#[derive(Debug, Default)]
struct Session {
    turn: u64,
    entries: Vec<Entry>,
}

/// Append-only conversation log.
///
/// Entries are appended by the loop (user/assistant) and by this plugin's own
/// sync listeners (tool results, state markers) — the listeners run inline on
/// the emitting thread, so a log append is guaranteed once the loop's call
/// returns.
pub struct SessionLog {
    ctx: Context,
    sessions: Mutex<HashMap<SessionId, Session>>,
}

impl SessionLog {
    pub fn new(ctx: Context) -> Self {
        SessionLog {
            ctx,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Opens a new turn, incrementing the per-session turn counter.
    /// Emits `session.turn_opened`.
    pub fn begin_turn(&self, session_id: &str) -> u64 {
        let turn = {
            let mut sessions = self.lock();
            let session = sessions.entry(session_id.to_owned()).or_default();
            session.turn += 1;
            session.turn
        };
        let _ = self.ctx.emit_key(
            CH_SESSION_TURN_OPENED,
            SessionTurnOpened {
                session_id: session_id.to_owned(),
                turn,
            },
        );
        turn
    }

    /// Closes the current turn. Emits `session.turn_closed`.
    pub fn end_turn(&self, session_id: &str, turn: u64) {
        let _ = self.ctx.emit_key(
            CH_SESSION_TURN_CLOSED,
            SessionTurnClosed {
                session_id: session_id.to_owned(),
                turn,
            },
        );
    }

    /// Appends an entry. Emits `session.entry_appended`.
    pub fn append(&self, session_id: &str, turn: u64, entry: Entry) {
        {
            let mut sessions = self.lock();
            sessions
                .entry(session_id.to_owned())
                .or_default()
                .entries
                .push(entry.clone());
        }
        let _ = self.ctx.emit_key(
            CH_SESSION_ENTRY_APPENDED,
            SessionEntryAppended {
                session_id: session_id.to_owned(),
                turn,
                entry,
            },
        );
    }

    /// Snapshot of a session's entries.
    pub fn history(&self, session_id: &str) -> Vec<Entry> {
        self.lock()
            .get(session_id)
            .map(|s| s.entries.clone())
            .unwrap_or_default()
    }

    /// Number of entries recorded for a session.
    pub fn len(&self, session_id: &str) -> usize {
        self.history(session_id).len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Session>> {
        self.sessions.lock().expect("session log poisoned")
    }
}

pub struct SessionPlugin;

impl harness_core::Plugin for SessionPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("session")
            .provides(KEY_SESSIONS)
            .emits::<SessionTurnOpened>(CH_SESSION_TURN_OPENED)
            .emits::<SessionEntryAppended>(CH_SESSION_ENTRY_APPENDED)
            .emits::<SessionTurnClosed>(CH_SESSION_TURN_CLOSED)
            .listens::<ToolExecuted>(CH_TOOL_EXECUTED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let log = Arc::new(SessionLog::new(ctx.clone()));
        ctx.provide_key(KEY_SESSIONS, log.clone());

        // Tool results are appended inline as the tools plugin executes:
        // sync listeners run on the emitting thread, so the result is already
        // logged when `Tools::execute` returns to the loop.
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
    use harness_contracts::{Role, ToolCall, ToolError};

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
    fn history_of_unknown_session_is_empty() {
        let ctx = Context::root();
        ctx.load(SessionPlugin).unwrap();
        let log: Arc<SessionLog> = ctx.inject_key(KEY_SESSIONS).unwrap();
        assert!(log.history("nope").is_empty());
        assert_eq!(log.len("nope"), 0);
    }
}
