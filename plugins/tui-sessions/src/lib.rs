//! Session picker popup: the `/sessions` bridge between a filter popup
//! and the session catalog.
//!
//! The TUI shell stays domain-free: it draws the provider snapshot and
//! routes keys here while a picker session is live, then drains the staged
//! switch request and performs the actual switch itself (transcript reload
//! plus session-id swap). This plugin owns everything session-specific:
//! the trigger line, the search syntax, substring filtering over catalog
//! rows, and staging the resume pick. Listing behavior itself is the shared
//! [`FilterPopup`](harness_tui_filter::FilterPopup) driver, so the entry
//! bar stays live while searching, exactly like model search.
//!
//! Provided as `Arc<SessionPopup>` under
//! [`KEY_SESSION_POPUP`](harness_contracts::KEY_SESSION_POPUP). Injects
//! only the session catalog handle, so it parks until the session plugin
//! is loaded and can never observe any other provider's list.

use std::sync::{Arc, Mutex, MutexGuard};

use harness_contracts::{
    KEY_SESSION_CATALOG, KEY_SESSION_POPUP, SessionCatalogHandle, SessionSummary,
};
use harness_core::{Context, Result};
use harness_tui_filter::{FilterPopup, FilterSource};
use harness_tui_popup::ActivePopup;
use harness_tui_state::app::{App, KeyEvent};

/// Exact input line (trimmed) that opens the picker instead of submitting
/// as chat.
pub const TRIGGER: &str = "/sessions";

/// Popup title, with the surrounding spaces the border title expects.
pub const TITLE: &str = " sessions ";

/// Placeholder row when the catalog holds no sessions at all (e.g.
/// persistence disabled with nothing yet in-process). Picking it just
/// clears the line — it never stages a switch.
pub const PLACEHOLDER: &str = "(no past sessions)";

/// Title shown for sessions whose first user message was blank.
pub const UNTITLED: &str = "(untitled)";

/// Session-id prefix shown in picker rows: long enough to disambiguate,
/// short enough to keep rows readable.
const SHORT_ID_CHARS: usize = 8;

/// Extracts the session-search query from an input line: `/sessions` alone
/// or `/sessions <query>` (single line). Returns `None` for anything else,
/// which ends the picker session.
pub fn sessions_query(input: &str) -> Option<String> {
    let trimmed = input.trim_start();
    if trimmed.contains('\n') {
        return None;
    }
    let rest = trimmed.strip_prefix(TRIGGER)?;
    if rest.is_empty() {
        return Some(String::new());
    }
    match rest.chars().next() {
        Some(c) if c.is_whitespace() => Some(rest.trim().to_owned()),
        _ => None,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Human age ("12m ago") from epoch seconds. The contract layer carries
/// epoch secs precisely so no date crate is needed anywhere; coarse
/// buckets are all a picker needs.
fn age(updated_at: u64, now: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const MONTH: u64 = 30 * DAY;
    let secs = now.saturating_sub(updated_at);
    if secs < MINUTE {
        "just now".to_owned()
    } else if secs < HOUR {
        format!("{}m ago", secs / MINUTE)
    } else if secs < DAY {
        format!("{}h ago", secs / HOUR)
    } else if secs < MONTH {
        format!("{}d ago", secs / DAY)
    } else {
        format!("{}mo ago", secs / MONTH)
    }
}

/// One picker row: display label plus the session id it resumes. Labels
/// embed the id prefix, so equal titles still map back unambiguously.
#[derive(Debug, Clone)]
struct SessionRow {
    id: String,
    label: String,
}

fn render_row(summary: &SessionSummary, now: u64) -> SessionRow {
    let short: String = summary.id.chars().take(SHORT_ID_CHARS).collect();
    let title = if summary.title.is_empty() {
        UNTITLED
    } else {
        summary.title.as_str()
    };
    SessionRow {
        id: summary.id.clone(),
        label: format!("{title} · {} · {short}", age(summary.updated_at, now)),
    }
}

#[derive(Default)]
struct SessionState {
    rows: Vec<SessionRow>,
    pending: Option<String>,
    current: String,
}

/// Session source for the shared driver: `/sessions <query>` candidate,
/// case-insensitive substring matches, shared complete-unless-exact
/// `Enter`, and exact-`Enter` stages the resume pick for the shell.
#[derive(Clone)]
struct SessionSource {
    catalog: Arc<SessionCatalogHandle>,
    state: Arc<Mutex<SessionState>>,
}

impl SessionSource {
    fn lock(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl FilterSource for SessionSource {
    fn title(&self) -> &str {
        TITLE
    }

    fn candidate(&self, input: &str) -> Option<String> {
        sessions_query(input)
    }

    fn items(&self, query: &str) -> Vec<String> {
        let state = self.lock();
        if state.rows.is_empty() {
            return vec![PLACEHOLDER.to_owned()];
        }
        let query = query.to_lowercase();
        state
            .rows
            .iter()
            .filter(|row| row.label.to_lowercase().contains(&query))
            .map(|row| row.label.clone())
            .collect()
    }

    fn current(&self) -> Option<String> {
        let state = self.lock();
        state
            .rows
            .iter()
            .find(|row| row.id == state.current)
            .map(|row| row.label.clone())
    }

    fn initial_selected(&self, items: &[String]) -> usize {
        self.current()
            .and_then(|current| items.iter().position(|item| *item == current))
            .unwrap_or(0)
    }

    fn render_completion(&self, selected: &str) -> String {
        format!("{TRIGGER} {selected}")
    }

    fn on_dismiss(&self, app: &mut App) {
        app.clear_input();
    }

    fn on_exact(&self, selected: &str, app: &mut App) -> bool {
        // Exact `Enter` stages the resume pick: the label maps back to its
        // session id (the placeholder maps to nothing). The shell drains
        // the pick and performs the switch — this provider never touches
        // loop or transcript state.
        if selected != PLACEHOLDER {
            let mut state = self.lock();
            if let Some(row) = state.rows.iter().find(|row| row.label == selected) {
                state.pending = Some(row.id.clone());
            }
        }
        app.clear_input();
        true
    }
}

pub struct SessionPopup {
    filter: FilterPopup<SessionSource>,
    source: SessionSource,
}

impl SessionPopup {
    pub fn new(catalog: Arc<SessionCatalogHandle>) -> Self {
        let source = SessionSource {
            catalog,
            state: Arc::new(Mutex::new(SessionState::default())),
        };
        SessionPopup {
            filter: FilterPopup::new(source.clone()),
            source,
        }
    }

    /// Whether an input line should open the picker instead of submitting
    /// as chat.
    pub fn wants_input(&self, input: &str) -> bool {
        input.trim() == TRIGGER
    }

    /// Whether a picker session is live (independent of list visibility).
    /// The shell routes keys here — and suppresses slash completion —
    /// while this is true.
    pub fn is_active(&self) -> bool {
        self.filter.is_active()
    }

    pub fn is_open(&self) -> bool {
        self.filter.is_open()
    }

    pub fn snapshot(&self) -> Option<ActivePopup> {
        self.filter.snapshot()
    }

    /// Ends the session and hides the list.
    pub fn close(&self) {
        self.filter.close();
    }

    /// Opens the picker for an exact `/sessions` Enter: refreshes rows from
    /// the catalog (synchronous — the catalog is an in-memory index, so no
    /// background refresh like model search needs), stages `/sessions ` in
    /// the (still live) entry bar, and shows the list with the cursor on
    /// the current session.
    pub fn open(&self, app: &mut App) {
        let now = now_secs();
        let rows: Vec<SessionRow> = self
            .source
            .catalog
            .list()
            .iter()
            .map(|summary| render_row(summary, now))
            .collect();
        self.source.lock().rows = rows;
        app.set_input(&format!("{TRIGGER} "));
        self.filter.sync(app);
    }

    /// Reconciles the picker with the input line. No-op unless a session
    /// is live; editing away from the `/sessions` prefix ends it (the shell
    /// then hands the line back to slash completion).
    pub fn sync(&self, app: &App) {
        if !self.filter.is_active() {
            return;
        }
        self.filter.sync(app);
    }

    /// Handles one key via the shared driver. Returns `true` when consumed
    /// (never reaches `App`); editing keys fall through so the entry bar
    /// stays live and re-filters via the shell's post-reduce `sync`.
    /// `Interrupt` (Ctrl+C) returns `false` so the app can still quit.
    pub fn handle_key(&self, key: KeyEvent, app: &mut App) -> bool {
        if !self.filter.is_active() {
            return false;
        }
        self.filter.handle_key(key, app)
    }

    /// Drains the staged resume pick (`Some(session_id)`), if any. The
    /// shell calls this after every handled key and reduce, then performs
    /// the switch itself.
    pub fn take_switch_request(&self) -> Option<String> {
        self.source.lock().pending.take()
    }

    /// Marks the live session (● marker + initial cursor). Called by the
    /// shell on startup and after every switch.
    pub fn set_current(&self, session_id: &str) {
        self.source.lock().current = session_id.to_owned();
    }
}

/// Plugin providing the session picker bridge as `Arc<SessionPopup>` under
/// [`KEY_SESSION_POPUP`](harness_contracts::KEY_SESSION_POPUP).
pub struct TuiSessionsPlugin;

impl harness_core::Plugin for TuiSessionsPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui-sessions")
            .provides(KEY_SESSION_POPUP)
            .injects(KEY_SESSION_CATALOG)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let catalog: Arc<SessionCatalogHandle> = ctx.inject_key(KEY_SESSION_CATALOG)?;
        ctx.provide_key(KEY_SESSION_POPUP, Arc::new(SessionPopup::new(catalog)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::SessionCatalogApi;
    use harness_tui_state::app::AppMsg;

    struct FakeCatalog {
        summaries: Mutex<Vec<SessionSummary>>,
    }

    impl SessionCatalogApi for FakeCatalog {
        fn list(&self) -> Vec<SessionSummary> {
            self.summaries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }

    fn summary(id: &str, title: &str, updated_at: u64) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            title: title.into(),
            created_at: updated_at,
            updated_at,
            turns: 1,
            entries: 2,
        }
    }

    fn popup_with(summaries: Vec<SessionSummary>) -> SessionPopup {
        let catalog = Arc::new(SessionCatalogHandle(Arc::new(FakeCatalog {
            summaries: Mutex::new(summaries),
        })));
        SessionPopup::new(catalog)
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
    }

    /// Drives one shell-style key: the picker consumes nav/completion keys,
    /// everything else falls through to `App`, then the picker re-syncs.
    /// Drains no switch request (switch tests drain explicitly).
    fn press(picker: &SessionPopup, app: &mut App, key: KeyEvent) {
        if !picker.handle_key(key, app) {
            app.update(AppMsg::Key(key));
        }
        picker.sync(app);
    }

    #[test]
    fn wants_input_matches_exact_trigger_only() {
        let picker = popup_with(vec![]);
        assert!(picker.wants_input("/sessions"));
        assert!(picker.wants_input("  /sessions  "));
        assert!(!picker.wants_input("/sessions x"));
        assert!(!picker.wants_input("/sessionsx"));
        assert!(!picker.wants_input("/session"));
        assert!(!picker.wants_input("hello"));
    }

    #[test]
    fn sessions_query_parses_search_syntax() {
        assert_eq!(sessions_query("/sessions"), Some(String::new()));
        assert_eq!(sessions_query("/sessions "), Some(String::new()));
        assert_eq!(sessions_query("  /sessions  "), Some(String::new()));
        assert_eq!(
            sessions_query("/sessions debug"),
            Some("debug".to_owned())
        );
        assert_eq!(
            sessions_query("/sessions  debug  "),
            Some("debug".to_owned())
        );
        assert_eq!(sessions_query("/sessionsx"), None);
        assert_eq!(sessions_query("/sessionsx y"), None);
        assert_eq!(sessions_query("/session"), None);
        assert_eq!(sessions_query("hello"), None);
        assert_eq!(sessions_query(""), None);
        assert_eq!(sessions_query("/sessions\ndbg"), None);
    }

    #[test]
    fn age_buckets_are_coarse() {
        assert_eq!(age(1000, 1000), "just now");
        assert_eq!(age(1000, 1059), "just now");
        assert_eq!(age(1000, 1060), "1m ago");
        assert_eq!(age(0, 3599), "59m ago");
        assert_eq!(age(0, 3600), "1h ago");
        assert_eq!(age(0, 86399), "23h ago");
        assert_eq!(age(0, 86400), "1d ago");
        assert_eq!(age(0, 29 * 86400), "29d ago");
        assert_eq!(age(0, 30 * 86400), "1mo ago");
        // Future timestamps (clock skew) clamp instead of underflowing.
        assert_eq!(age(2000, 1000), "just now");
    }

    #[test]
    fn open_stages_picker_line_and_lists_newest_first() {
        let picker = popup_with(vec![
            summary("aaa", "first chat", 1000),
            summary("bbb", "second chat", 2000),
        ]);
        picker.set_current("aaa");
        let mut app = App::new();
        type_text(&mut app, "/sessions");
        picker.open(&mut app);

        assert_eq!(app.input(), "/sessions ");
        assert!(picker.is_active());
        assert!(picker.is_open());
        let snap = picker.snapshot().expect("open");
        assert_eq!(snap.items.len(), 2);
        assert!(snap.items[0].starts_with("first chat · "));
        assert!(snap.items[0].ends_with(" · aaa"));
        assert!(snap.items[1].starts_with("second chat · "));
        // Cursor starts on the current session.
        assert_eq!(snap.selected, 0);
        assert_eq!(snap.current.as_deref(), Some(snap.items[0].as_str()));
    }

    #[test]
    fn typing_filters_and_entry_stays_live() {
        let picker = popup_with(vec![
            summary("aaa", "debug auth", 1000),
            summary("bbb", "write docs", 2000),
        ]);
        let mut app = App::new();
        picker.open(&mut app);

        for ch in "DOCS".chars() {
            press(&picker, &mut app, KeyEvent::Char(ch));
        }
        // Case-insensitive substring match; entry bar untouched.
        assert_eq!(app.input(), "/sessions DOCS");
        let snap = picker.snapshot().expect("open");
        assert_eq!(snap.items.len(), 1);
        assert!(snap.items[0].starts_with("write docs · "));

        for _ in 0..4 {
            press(&picker, &mut app, KeyEvent::Backspace);
        }
        assert_eq!(app.input(), "/sessions ");
        assert_eq!(picker.snapshot().expect("open").items.len(), 2);
    }

    #[test]
    fn exact_enter_stages_switch_and_clears() {
        let picker = popup_with(vec![
            summary("aaa", "debug auth", 1000),
            summary("bbb", "write docs", 2000),
        ]);
        picker.set_current("aaa");
        let mut app = App::new();
        picker.open(&mut app);

        // Narrow to one row, complete it, then apply with exact Enter.
        for ch in "docs".chars() {
            press(&picker, &mut app, KeyEvent::Char(ch));
        }
        assert!(picker.handle_key(KeyEvent::Enter, &mut app));
        assert!(app.input().starts_with("/sessions write docs · "));
        assert!(picker.take_switch_request().is_none(), "complete only");

        assert!(picker.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(picker.take_switch_request().as_deref(), Some("bbb"));
        // Drained: second take finds nothing.
        assert!(picker.take_switch_request().is_none());
        // Input cleared and the picker session ended.
        assert_eq!(app.input(), "");
        assert!(!picker.is_active());
    }

    #[test]
    fn esc_dismisses_and_clears_staged_line() {
        let picker = popup_with(vec![summary("aaa", "debug", 1000)]);
        let mut app = App::new();
        picker.open(&mut app);
        type_text(&mut app, "debug");
        picker.sync(&app);
        assert!(picker.is_open());

        assert!(picker.handle_key(KeyEvent::Esc, &mut app));

        assert!(!picker.is_active());
        assert!(!picker.is_open());
        assert_eq!(app.input(), "");
        assert!(picker.take_switch_request().is_none());
    }

    #[test]
    fn editing_away_ends_session() {
        let picker = popup_with(vec![summary("aaa", "debug", 1000)]);
        let mut app = App::new();
        picker.open(&mut app);
        assert!(picker.is_active());

        for _ in 0..2 {
            press(&picker, &mut app, KeyEvent::Backspace);
        }
        assert_eq!(app.input(), "/session");
        assert!(!picker.is_active());
        assert!(!picker.is_open());
    }

    #[test]
    fn empty_catalog_shows_placeholder_without_switch() {
        let picker = popup_with(vec![]);
        let mut app = App::new();
        picker.open(&mut app);

        assert!(picker.is_open());
        let snap = picker.snapshot().expect("open");
        assert_eq!(snap.items, vec![PLACEHOLDER.to_owned()]);

        // Completing the placeholder is a no-op for the input line...
        assert!(picker.handle_key(KeyEvent::Enter, &mut app));
        assert!(app.input().starts_with("/sessions "));
        // ...and exact-Enter on it clears without staging a switch.
        assert!(picker.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(app.input(), "");
        assert!(picker.take_switch_request().is_none());
    }

    #[test]
    fn untitled_sessions_render_without_blank_titles() {
        let picker = popup_with(vec![summary("aaa", "", 1000)]);
        let mut app = App::new();
        picker.open(&mut app);
        let snap = picker.snapshot().expect("open");
        assert!(snap.items[0].starts_with("(untitled) · "));
    }

    #[test]
    fn sync_without_session_is_noop() {
        // Typing `/sessions` must never auto-open the picker: picker
        // sessions start only via an explicit `open`.
        let picker = popup_with(vec![summary("aaa", "debug", 1000)]);
        let mut app = App::new();
        type_text(&mut app, "/sessions");
        picker.sync(&app);
        assert!(!picker.is_active());
        assert!(!picker.is_open());
    }

    #[test]
    fn handle_key_without_session_falls_through() {
        let picker = popup_with(vec![]);
        let mut app = App::new();
        assert!(!picker.handle_key(KeyEvent::Up, &mut app));
        assert!(!picker.handle_key(KeyEvent::Enter, &mut app));
    }

    #[test]
    fn interrupt_falls_through_so_app_can_quit() {
        let picker = popup_with(vec![summary("aaa", "debug", 1000)]);
        let mut app = App::new();
        picker.open(&mut app);
        assert!(!picker.handle_key(KeyEvent::Interrupt, &mut app));
    }

    #[test]
    fn set_current_moves_marker_and_cursor() {
        let picker = popup_with(vec![
            summary("aaa", "first", 1000),
            summary("bbb", "second", 2000),
        ]);
        picker.set_current("bbb");
        let mut app = App::new();
        picker.open(&mut app);
        let snap = picker.snapshot().expect("open");
        assert_eq!(snap.selected, 1);
        assert_eq!(snap.current.as_deref(), Some(snap.items[1].as_str()));
    }

    #[test]
    fn plugin_provides_picker_through_di() {
        struct FakeCatalogPlugin;

        impl harness_core::Plugin for FakeCatalogPlugin {
            fn meta(&self) -> harness_core::PluginMeta {
                harness_core::PluginMeta::new("fake-catalog")
                    .provides(KEY_SESSION_CATALOG)
            }

            fn build(&self, ctx: Context) -> Result<()> {
                let catalog = Arc::new(SessionCatalogHandle(Arc::new(FakeCatalog {
                    summaries: Mutex::new(vec![]),
                })));
                ctx.provide_key(KEY_SESSION_CATALOG, catalog);
                Ok(())
            }
        }

        let ctx = Context::root();
        ctx.load(FakeCatalogPlugin).unwrap();
        ctx.load(TuiSessionsPlugin).unwrap();
        let picker: Arc<SessionPopup> = ctx.inject_key(KEY_SESSION_POPUP).unwrap();
        assert!(!picker.is_active());
        assert!(picker.wants_input("/sessions"));
    }

    #[test]
    fn plugin_parks_until_catalog_is_loaded() {
        let ctx = Context::root();
        let outcome = ctx.load(TuiSessionsPlugin).unwrap();
        assert!(matches!(outcome, harness_core::LoadOutcome::Pending { .. }));
        assert!(ctx.provider_of(&KEY_SESSION_POPUP.into()).is_none());
    }
}
