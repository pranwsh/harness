//! Branch navigator popup: the `/branch` bridge between a filter popup
//! and the session turn tree.
//!
//! The TUI shell stays domain-free: it draws the provider snapshot and
//! routes keys here while a navigator session is live, then drains the
//! staged revert request and performs the actual rewind itself (head move
//! plus transcript reload). This plugin owns everything branch-specific:
//! the trigger line, the search syntax, substring filtering over tree
//! rows, depth indentation, and staging the revert pick. Listing behavior
//! itself is the shared [`FilterPopup`](harness_tui_filter::FilterPopup)
//! driver, so the entry bar stays live while searching, exactly like the
//! sessions picker.
//!
//! Provided as `Arc<BranchPopup>` under
//! [`KEY_BRANCH_POPUP`](harness_contracts::KEY_BRANCH_POPUP). Injects
//! only the session tree handle, so it parks until the session plugin
//! is loaded and can never observe any other provider's list.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use harness_contracts::{BranchNode, KEY_BRANCH_POPUP, KEY_SESSION_TREE, SessionTreeHandle};
use harness_core::{Context, Result};
use harness_tui_filter::{FilterPopup, FilterSource};
use harness_tui_popup::ActivePopup;
use harness_tui_state::app::{App, KeyEvent};

/// Exact input line (trimmed) that opens the navigator instead of
/// submitting as chat.
pub const TRIGGER: &str = "/branch";

/// Popup title, with the surrounding spaces the border title expects.
pub const TITLE: &str = " branch ";

/// Placeholder row when the session has no turns yet. Picking it just
/// clears the line — it never stages a revert.
pub const PLACEHOLDER: &str = "(no turns yet)";

/// Title shown for turns whose first user message was blank.
pub const UNTITLED: &str = "(untitled)";

/// Spaces per tree depth level.
const INDENT: &str = "  ";

/// Marker for a turn that starts a side branch (a non-first child).
const FORK_MARK: &str = "↳ ";

pub use harness_contracts::SessionTreeApi;

/// Extracts the branch-search query from an input line: `/branch` alone
/// or `/branch <query>` (single line). Returns `None` for anything else,
/// which ends the navigator session.
pub fn branch_query(input: &str) -> Option<String> {
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

/// One navigator row: display label plus the turn it rewinds to. Labels
/// embed the turn id, so equal titles still map back unambiguously.
#[derive(Debug, Clone)]
struct BranchRow {
    turn: u64,
    label: String,
    /// Off-head turns render dimmed: kept, visible, never deleted.
    dim: bool,
}

fn render_label(depth: usize, fork: bool, node: &BranchNode) -> String {
    let title = if node.title.is_empty() {
        UNTITLED
    } else {
        node.title.as_str()
    };
    let entries = if node.entries == 1 {
        "entry"
    } else {
        "entries"
    };
    format!(
        "{}{}{} t{} · {} {}",
        INDENT.repeat(depth),
        if fork { FORK_MARK } else { "" },
        title,
        node.turn,
        node.entries,
        entries,
    )
}

/// Lays out turn nodes as indented rows: the first child continues its
/// parent's depth, later children (and their subtrees) indent one level
/// and carry the fork marker. Nodes whose parent is missing hang off the
/// root. Input order is irrelevant. Siblings render fork-first: later
/// children (ascending by turn) directly under the parent, then the
/// earliest child (the flat mainline) last; roots stay ascending by turn.
fn build_rows(nodes: &[BranchNode]) -> Vec<BranchRow> {
    let mut children: HashMap<u64, Vec<&BranchNode>> = HashMap::new();
    let mut roots: Vec<&BranchNode> = Vec::new();
    let known: std::collections::HashSet<u64> = nodes.iter().map(|n| n.turn).collect();
    for node in nodes {
        if node.parent == 0 || !known.contains(&node.parent) {
            roots.push(node);
        } else {
            children.entry(node.parent).or_default().push(node);
        }
    }
    roots.sort_by_key(|n| n.turn);
    for group in children.values_mut() {
        group.sort_by_key(|n| n.turn);
    }
    let mut rows = Vec::with_capacity(nodes.len());
    let mut stack: Vec<(&BranchNode, usize, bool)> =
        roots.into_iter().map(|n| (n, 0, false)).collect();
    // Roots pushed ascending, so pop from the back after reversing.
    stack.reverse();
    while let Some((node, depth, fork)) = stack.pop() {
        rows.push(BranchRow {
            turn: node.turn,
            label: render_label(depth, fork, node),
            dim: !node.active,
        });
        if let Some(group) = children.remove(&node.turn) {
            // Fork-first: mainline (group[0]) pushed first so it pops last;
            // forks pushed after (in reverse) so they pop ascending directly
            // under the parent. Identity is stable: earliest turn stays flat.
            stack.push((group[0], depth, false));
            for child in group.into_iter().skip(1).rev() {
                stack.push((child, depth + 1, true));
            }
        }
    }
    rows
}

#[derive(Default)]
struct BranchState {
    rows: Vec<BranchRow>,
    pending: Option<u64>,
    session_id: String,
    head: u64,
}

/// Branch source for the shared driver: `/branch <query>` candidate,
/// case-insensitive substring matches, shared complete-unless-exact
/// `Enter`, and exact-`Enter` stages the revert pick for the shell.
/// `dim` runs the same filter as `items` so flags stay parallel.
#[derive(Clone)]
struct BranchSource {
    tree: Arc<SessionTreeHandle>,
    state: Arc<Mutex<BranchState>>,
}

impl BranchSource {
    fn lock(&self) -> MutexGuard<'_, BranchState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn filtered(&self, query: &str) -> (Vec<String>, Vec<bool>) {
        let state = self.lock();
        if state.rows.is_empty() {
            return (vec![PLACEHOLDER.to_owned()], vec![false]);
        }
        let query = query.to_lowercase();
        state
            .rows
            .iter()
            .filter(|row| row.label.to_lowercase().contains(&query))
            .map(|row| (row.label.clone(), row.dim))
            .unzip()
    }
}

impl FilterSource for BranchSource {
    fn title(&self) -> &str {
        TITLE
    }

    fn candidate(&self, input: &str) -> Option<String> {
        branch_query(input)
    }

    fn items(&self, query: &str) -> Vec<String> {
        self.filtered(query).0
    }

    fn dim(&self, query: &str) -> Vec<bool> {
        self.filtered(query).1
    }

    fn current(&self) -> Option<String> {
        let state = self.lock();
        state
            .rows
            .iter()
            .find(|row| row.turn == state.head)
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
        // Exact `Enter` stages the revert pick: the label maps back to its
        // turn (the placeholder maps to nothing). The shell drains the
        // pick and performs the rewind — this provider never touches loop
        // or transcript state.
        if selected != PLACEHOLDER {
            let mut state = self.lock();
            if let Some(row) = state.rows.iter().find(|row| row.label == selected) {
                state.pending = Some(row.turn);
            }
        }
        app.clear_input();
        true
    }
}

pub struct BranchPopup {
    filter: FilterPopup<BranchSource>,
    source: BranchSource,
}

impl BranchPopup {
    pub fn new(tree: Arc<SessionTreeHandle>) -> Self {
        let source = BranchSource {
            tree,
            state: Arc::new(Mutex::new(BranchState::default())),
        };
        BranchPopup {
            filter: FilterPopup::new(source.clone()),
            source,
        }
    }

    /// Whether an input line should open the navigator instead of
    /// submitting as chat.
    pub fn wants_input(&self, input: &str) -> bool {
        input.trim() == TRIGGER
    }

    /// Whether a navigator session is live (independent of list
    /// visibility). The shell routes keys here — and suppresses slash
    /// completion — while this is true.
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

    /// Opens the navigator for an exact `/branch` Enter: refreshes rows
    /// from the tree (synchronous — the tree is in-memory), stages
    /// `/branch ` in the (still live) entry bar, and shows the list with
    /// the cursor on the head turn.
    pub fn open(&self, app: &mut App) {
        let session_id = self.source.lock().session_id.clone();
        let nodes = self.source.tree.tree(&session_id);
        let head = self.source.tree.head(&session_id);
        let mut state = self.source.lock();
        state.rows = build_rows(&nodes);
        state.head = head;
        state.pending = None;
        drop(state);
        app.set_input(&format!("{TRIGGER} "));
        self.filter.sync(app);
    }

    /// Reconciles the navigator with the input line. No-op unless a
    /// session is live; editing away from the `/branch` prefix ends it
    /// (the shell then hands the line back to slash completion).
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

    /// Drains the staged revert pick (`Some(turn)`), if any. The shell
    /// calls this after every handled key and reduce, then performs the
    /// rewind itself.
    pub fn take_revert_request(&self) -> Option<u64> {
        self.source.lock().pending.take()
    }

    /// Retargets the navigator at a session. Called by the shell on
    /// startup and after every session switch.
    pub fn set_session(&self, session_id: &str) {
        self.source.lock().session_id = session_id.to_owned();
    }
}

/// Plugin providing the branch navigator bridge as `Arc<BranchPopup>`
/// under [`KEY_BRANCH_POPUP`](harness_contracts::KEY_BRANCH_POPUP).
pub struct TuiBranchPlugin;

impl harness_core::Plugin for TuiBranchPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui-branch")
            .provides(KEY_BRANCH_POPUP)
            .injects(KEY_SESSION_TREE)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let tree: Arc<SessionTreeHandle> = ctx.inject_key(KEY_SESSION_TREE)?;
        ctx.provide_key(KEY_BRANCH_POPUP, Arc::new(BranchPopup::new(tree)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_tui_state::app::AppMsg;
    use std::sync::Mutex as StdMutex;

    struct FakeTree {
        nodes: StdMutex<Vec<BranchNode>>,
        head: StdMutex<u64>,
    }

    impl SessionTreeApi for FakeTree {
        fn tree(&self, _session_id: &str) -> Vec<BranchNode> {
            self.nodes.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
        fn head(&self, _session_id: &str) -> u64 {
            *self.head.lock().unwrap_or_else(|e| e.into_inner())
        }
        fn revert(&self, _session_id: &str, turn: u64) -> bool {
            let nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
            if !nodes.iter().any(|n| n.turn == turn) {
                return false;
            }
            *self.head.lock().unwrap_or_else(|e| e.into_inner()) = turn;
            true
        }
    }

    fn node(turn: u64, parent: u64, title: &str, active: bool) -> BranchNode {
        BranchNode {
            turn,
            parent,
            title: title.into(),
            entries: 2,
            active,
        }
    }

    fn popup_with(nodes: Vec<BranchNode>, head: u64) -> BranchPopup {
        let tree = Arc::new(SessionTreeHandle(Arc::new(FakeTree {
            nodes: StdMutex::new(nodes),
            head: StdMutex::new(head),
        })));
        let popup = BranchPopup::new(tree);
        popup.set_session("s");
        popup
    }

    fn linear() -> Vec<BranchNode> {
        vec![
            node(1, 0, "first", true),
            node(2, 1, "second", true),
            node(3, 2, "third", false),
        ]
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
    }

    /// Drives one shell-style key: the navigator consumes nav/completion
    /// keys, everything else falls through to `App`, then re-syncs.
    fn press(picker: &BranchPopup, app: &mut App, key: KeyEvent) {
        if !picker.handle_key(key, app) {
            app.update(AppMsg::Key(key));
        }
        picker.sync(app);
    }

    #[test]
    fn wants_input_matches_exact_trigger_only() {
        let picker = popup_with(vec![], 0);
        assert!(picker.wants_input("/branch"));
        assert!(picker.wants_input("  /branch  "));
        assert!(!picker.wants_input("/branch x"));
        assert!(!picker.wants_input("/branchx"));
        assert!(!picker.wants_input("/bran"));
        assert!(!picker.wants_input("hello"));
    }

    #[test]
    fn branch_query_parses_search_syntax() {
        assert_eq!(branch_query("/branch"), Some(String::new()));
        assert_eq!(branch_query("/branch "), Some(String::new()));
        assert_eq!(branch_query("  /branch  "), Some(String::new()));
        assert_eq!(branch_query("/branch debug"), Some("debug".to_owned()));
        assert_eq!(branch_query("/branch  debug  "), Some("debug".to_owned()));
        assert_eq!(branch_query("/branchx"), None);
        assert_eq!(branch_query("/branchx y"), None);
        assert_eq!(branch_query("/bran"), None);
        assert_eq!(branch_query("hello"), None);
        assert_eq!(branch_query(""), None);
        assert_eq!(branch_query("/branch\ndbg"), None);
    }

    #[test]
    fn build_rows_marks_forks_and_keeps_linear_flat() {
        let rows = build_rows(&[
            node(1, 0, "root", true),
            node(2, 1, "kept", true),
            node(3, 1, "abandoned", false),
            node(4, 3, "revived", true),
        ]);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].label, "root t1 · 2 entries");
        assert!(!rows[0].dim);
        // Fork-first: the later child sits directly under the parent even
        // though the mainline turn is earlier; mainline renders last.
        assert_eq!(
            rows[1].label,
            format!("{INDENT}{FORK_MARK}abandoned t3 · 2 entries")
        );
        assert!(rows[1].dim);
        // Its subtree keeps the fork depth.
        assert_eq!(rows[2].label, format!("{INDENT}revived t4 · 2 entries"));
        assert!(!rows[2].dim);
        assert_eq!(rows[3].label, "kept t2 · 2 entries");
        assert!(!rows[3].dim);
    }

    #[test]
    fn late_fork_sits_directly_under_parent() {
        // t4 branches from t1 after the t2->t3 mainline already exists.
        let rows = build_rows(&[
            node(1, 0, "root", true),
            node(2, 1, "second", true),
            node(3, 2, "third", true),
            node(4, 1, "late-fork", false),
        ]);
        let turns: Vec<u64> = rows.iter().map(|r| r.turn).collect();
        assert_eq!(turns, vec![1, 4, 2, 3]);
        assert_eq!(
            rows[1].label,
            format!("{INDENT}{FORK_MARK}late-fork t4 · 2 entries")
        );
        assert_eq!(rows[2].label, "second t2 · 2 entries");
        assert_eq!(rows[3].label, "third t3 · 2 entries");
    }

    #[test]
    fn multiple_forks_stay_turn_ordered_before_mainline() {
        let rows = build_rows(&[
            node(1, 0, "root", true),
            node(2, 1, "main", true),
            node(3, 1, "fork-a", false),
            node(4, 1, "fork-b", false),
        ]);
        let turns: Vec<u64> = rows.iter().map(|r| r.turn).collect();
        assert_eq!(turns, vec![1, 3, 4, 2]);
    }

    #[test]
    fn nested_forks_render_before_nested_mainline() {
        // Fork-first applies recursively: under t2 the late fork t5 sits
        // directly under t2 ahead of the earlier child t3 (and its child t4).
        let rows = build_rows(&[
            node(1, 0, "root", true),
            node(2, 1, "second", true),
            node(3, 2, "third", true),
            node(4, 3, "fourth", true),
            node(5, 2, "nested-fork", false),
        ]);
        let turns: Vec<u64> = rows.iter().map(|r| r.turn).collect();
        assert_eq!(turns, vec![1, 2, 5, 3, 4]);
        assert_eq!(
            rows[2].label,
            format!("{INDENT}{FORK_MARK}nested-fork t5 · 2 entries")
        );
    }

    #[test]
    fn open_lists_tree_with_cursor_on_head() {
        let picker = popup_with(linear(), 2);
        let mut app = App::new();
        type_text(&mut app, "/branch");
        picker.open(&mut app);

        assert_eq!(app.input(), "/branch ");
        assert!(picker.is_active());
        assert!(picker.is_open());
        let snap = picker.snapshot().expect("open");
        assert_eq!(snap.items.len(), 3);
        assert!(snap.items[0].contains("first t1"));
        assert!(snap.items[2].contains("third t3"));
        // Dim flags travel parallel to items: only the off-head tail.
        assert_eq!(snap.dim, vec![false, false, true]);
        // Cursor starts on the head turn.
        assert_eq!(snap.selected, 1);
        assert_eq!(snap.current.as_deref(), Some(snap.items[1].as_str()));
    }

    #[test]
    fn typing_filters_and_entry_stays_live() {
        let picker = popup_with(linear(), 3);
        let mut app = App::new();
        picker.open(&mut app);

        for ch in "SECOND".chars() {
            press(&picker, &mut app, KeyEvent::Char(ch));
        }
        assert_eq!(app.input(), "/branch SECOND");
        let snap = picker.snapshot().expect("open");
        assert_eq!(snap.items.len(), 1);
        assert!(snap.items[0].contains("second t2"));
        assert_eq!(snap.dim, vec![false]);

        for _ in 0..6 {
            press(&picker, &mut app, KeyEvent::Backspace);
        }
        assert_eq!(app.input(), "/branch ");
        assert_eq!(picker.snapshot().expect("open").items.len(), 3);
    }

    #[test]
    fn exact_enter_stages_revert_and_clears() {
        let picker = popup_with(linear(), 3);
        let mut app = App::new();
        picker.open(&mut app);

        for ch in "first".chars() {
            press(&picker, &mut app, KeyEvent::Char(ch));
        }
        assert!(picker.handle_key(KeyEvent::Enter, &mut app));
        assert!(app.input().contains("first t1"));
        assert!(picker.take_revert_request().is_none(), "complete only");

        assert!(picker.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(picker.take_revert_request(), Some(1));
        assert!(picker.take_revert_request().is_none(), "drained");
        assert_eq!(app.input(), "");
        assert!(!picker.is_active());
    }

    #[test]
    fn esc_dismisses_and_clears_staged_line() {
        let picker = popup_with(linear(), 3);
        let mut app = App::new();
        picker.open(&mut app);
        type_text(&mut app, "first");
        picker.sync(&app);
        assert!(picker.is_open());

        assert!(picker.handle_key(KeyEvent::Esc, &mut app));

        assert!(!picker.is_active());
        assert!(!picker.is_open());
        assert_eq!(app.input(), "");
        assert!(picker.take_revert_request().is_none());
    }

    #[test]
    fn editing_away_ends_session() {
        let picker = popup_with(linear(), 3);
        let mut app = App::new();
        picker.open(&mut app);
        assert!(picker.is_active());

        for _ in 0..2 {
            press(&picker, &mut app, KeyEvent::Backspace);
        }
        assert_eq!(app.input(), "/branc");
        assert!(!picker.is_active());
        assert!(!picker.is_open());
    }

    #[test]
    fn empty_tree_shows_placeholder_without_revert() {
        let picker = popup_with(vec![], 0);
        let mut app = App::new();
        picker.open(&mut app);

        assert!(picker.is_open());
        let snap = picker.snapshot().expect("open");
        assert_eq!(snap.items, vec![PLACEHOLDER.to_owned()]);

        assert!(picker.handle_key(KeyEvent::Enter, &mut app));
        assert!(app.input().starts_with("/branch "));
        assert!(picker.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(app.input(), "");
        assert!(picker.take_revert_request().is_none());
    }

    #[test]
    fn untitled_turns_render_without_blank_titles() {
        let picker = popup_with(vec![node(1, 0, "", true)], 1);
        let mut app = App::new();
        picker.open(&mut app);
        let snap = picker.snapshot().expect("open");
        assert!(snap.items[0].starts_with("(untitled) t1"));
    }

    #[test]
    fn sync_without_session_is_noop() {
        let picker = popup_with(linear(), 3);
        let mut app = App::new();
        type_text(&mut app, "/branch");
        picker.sync(&app);
        assert!(!picker.is_active());
        assert!(!picker.is_open());
    }

    #[test]
    fn handle_key_without_session_falls_through() {
        let picker = popup_with(vec![], 0);
        let mut app = App::new();
        assert!(!picker.handle_key(KeyEvent::Up, &mut app));
        assert!(!picker.handle_key(KeyEvent::Enter, &mut app));
    }

    #[test]
    fn interrupt_falls_through_so_app_can_quit() {
        let picker = popup_with(linear(), 3);
        let mut app = App::new();
        picker.open(&mut app);
        assert!(!picker.handle_key(KeyEvent::Interrupt, &mut app));
    }

    #[test]
    fn plugin_provides_navigator_through_di() {
        struct FakeTreePlugin;

        impl harness_core::Plugin for FakeTreePlugin {
            fn meta(&self) -> harness_core::PluginMeta {
                harness_core::PluginMeta::new("fake-tree").provides(KEY_SESSION_TREE)
            }

            fn build(&self, ctx: Context) -> Result<()> {
                let tree = Arc::new(SessionTreeHandle(Arc::new(FakeTree {
                    nodes: StdMutex::new(vec![]),
                    head: StdMutex::new(0),
                })));
                ctx.provide_key(KEY_SESSION_TREE, tree);
                Ok(())
            }
        }

        let ctx = Context::root();
        ctx.load(FakeTreePlugin).unwrap();
        ctx.load(TuiBranchPlugin).unwrap();
        let picker: Arc<BranchPopup> = ctx.inject_key(KEY_BRANCH_POPUP).unwrap();
        assert!(!picker.is_active());
        assert!(picker.wants_input("/branch"));
    }

    #[test]
    fn plugin_parks_until_tree_is_loaded() {
        let ctx = Context::root();
        let outcome = ctx.load(TuiBranchPlugin).unwrap();
        assert!(matches!(outcome, harness_core::LoadOutcome::Pending { .. }));
        assert!(ctx.provider_of(&KEY_BRANCH_POPUP.into()).is_none());
    }
}
