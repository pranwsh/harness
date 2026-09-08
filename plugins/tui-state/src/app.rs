//! Pure application state: the chat transcript, input editor, scroll
//! position, and quit flag. Knows nothing about terminals or ratatui.

use harness_contracts::{ToolCall, ToolError};

use crate::editor::Editor;

/// How many content rows the page keys move the chat viewport by.
pub const PAGE_ROWS: usize = 10;

/// How many content rows one mouse-wheel notch moves the chat
/// viewport by: a single row for smooth, precise scrolling.
pub const WHEEL_ROWS: usize = 1;

/// Highest scroll offset (rows hidden below the viewport) that still
/// shows content: at the limit the earliest row sits one row below the
/// viewport top with the rest packed under it. Zero when everything
/// fits, so the earliest row can never sink to the viewport bottom.
pub fn max_chat_scroll(total_rows: usize, viewport_rows: usize) -> usize {
    if total_rows > viewport_rows {
        total_rows - viewport_rows + 1
    } else {
        0
    }
}

/// How many wrapped text rows the input box shows at most; the box
/// grows from 1 up to this many rows before scrolling its content.
pub const INPUT_VISIBLE_ROWS: usize = 3;

/// Fallback input width before the first draw reports the real one.
const DEFAULT_INPUT_WIDTH: usize = 80;

/// One rendered line of chat, already flattened to styled text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatItem {
    /// Content to display (may span several visual rows when wrapped).
    pub text: String,
    pub kind: ItemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    User,
    Assistant,
    ToolStarted,
    ToolOk,
    ToolErr,
    Notice,
    Error,
}

impl ChatItem {
    fn new(text: impl Into<String>, kind: ItemKind) -> Self {
        ChatItem {
            text: text.into(),
            kind,
        }
    }
}

/// Messages the runtime feeds into the app. Everything the UI reacts to
/// funnels through this enum, keeping `App` a deterministic reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMsg {
    /// A keypress from the terminal input task.
    Key(KeyEvent),
    /// An assistant text chunk arrived on a turn stream.
    Assistant(String),
    /// A streamed assistant text slice: appended to the trailing assistant
    /// item when there is one, otherwise starts a new one.
    AssistantDelta(String),
    /// A tool call started on a turn stream.
    ToolStarted(ToolCall),
    /// A tool call finished on a turn stream.
    ToolResult(ToolCall, Result<String, ToolError>),
    /// A turn finished successfully with its iteration count.
    Completed(u32),
    /// A turn failed.
    Failed(String),
    /// A turn's stream ended (always sent exactly once per spawned
    /// turn, after `Completed`/`Failed`); releases the busy slot.
    TurnFinished,
    /// A non-turn system line (e.g. `/model` changed the active model).
    Notice(String),
    /// A mouse-wheel notch over the chat pane: scroll up by a step.
    ScrollUp,
    /// A mouse-wheel notch over the chat pane: scroll down by a step.
    ScrollDown,
}

/// Keys the app understands, translated by the input task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    Char(char),
    Enter,
    /// Shift+Enter (or Alt+Enter): insert a newline without submitting.
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// Ctrl+C: terminal interrupt; quits the app.
    Interrupt,
    Esc,
}

/// Outcome of applying a message: whether anything changed and, when
/// the user submitted a chat message, that message.
#[derive(Debug, Default)]
pub struct Effect {
    pub redraw: bool,
    pub submitted: Option<String>,
}

impl Effect {
    fn redraw() -> Self {
        Effect {
            redraw: true,
            submitted: None,
        }
    }
}

/// Pure UI state.
#[derive(Debug, Default)]
pub struct App {
    /// Turns currently streaming; drives the busy hint.
    pub live_turns: u32,
    items: Vec<ChatItem>,
    editor: Editor,
    /// Wrapped content rows hidden below the chat viewport. Zero means
    /// pinned to the newest content; anything above keeps the viewport
    /// in place as new items stream in. Row-based (not item-based) so
    /// scrolling moves smoothly one row at a time. Clamped to the
    /// displayable range whenever the view reports fresh geometry.
    scroll_rows: usize,
    /// Streaming follow anchor: while set, the view follows new content
    /// only until the latest submitted user message reaches the viewport
    /// top, then freezes there instead of scrolling lower. Armed on submit,
    /// cleared by any manual scroll, by `/clear`, and once no turn is live.
    anchor_follow: bool,
    /// Measured chat content height and viewport height in rows,
    /// reported by the view each draw.
    chat_total_rows: usize,
    chat_viewport_rows: usize,
    /// Wrap width of the input box in columns, reported by the view
    /// each draw (key handling needs it for visual cursor motion).
    input_width: usize,
    /// First visible wrapped row of the input box.
    input_scroll: usize,
    quit: bool,
}

impl App {
    pub fn new() -> Self {
        App {
            live_turns: 0,
            items: Vec::new(),
            editor: Editor::new(),
            scroll_rows: 0,
            chat_total_rows: 0,
            chat_viewport_rows: 0,
            input_width: DEFAULT_INPUT_WIDTH,
            input_scroll: 0,
            anchor_follow: false,
            quit: false,
        }
    }

    /// The transcript, oldest item first.
    pub fn items(&self) -> &[ChatItem] {
        &self.items
    }

    /// Current input text.
    pub fn input(&self) -> &str {
        self.editor.text()
    }

    /// Discards the current input line (used when `/model` is hijacked to
    /// open the popup instead of submitting as chat).
    pub fn clear_input(&mut self) {
        let _ = self.editor.take();
        self.input_scroll = 0;
        self.follow_input_cursor();
    }

    /// Byte cursor offset within the input, for render-time positioning.
    pub fn input_cursor(&self) -> usize {
        self.editor.cursor()
    }

    /// Visual `(row, column)` of the input cursor at the current input
    /// width. Column is in display columns from the row's start.
    pub fn input_cursor_visual(&self) -> (usize, usize) {
        self.editor.cursor_visual(self.input_width)
    }

    /// Wrapped rows the input text currently occupies.
    pub fn input_rows(&self) -> usize {
        self.editor.row_count(self.input_width)
    }

    /// First visible wrapped row of the input box.
    pub fn input_scroll(&self) -> usize {
        self.input_scroll
    }

    /// Records the input box's wrap width (called by the view each
    /// draw) and re-clamps the input scroll to the new geometry.
    pub fn set_input_width(&mut self, width: usize) {
        self.input_width = width.max(1);
        self.clamp_input_scroll();
        self.follow_input_cursor();
    }

    /// Wrapped content rows hidden below the chat viewport. Zero pins
    /// the view to the newest content.
    pub fn scroll_rows(&self) -> usize {
        self.scroll_rows
    }

    /// Whether the streaming follow anchor is armed (see the field docs).
    pub fn anchor_follow(&self) -> bool {
        self.anchor_follow
    }

    /// Sets the scroll offset directly (called by the view each draw to
    /// enforce the streaming anchor), clamped to the displayable range.
    pub fn set_scroll_rows(&mut self, rows: usize) {
        self.scroll_rows = rows.min(self.max_chat_scroll());
    }

    /// Records the measured chat geometry (called by the view each
    /// draw) and clamps the scroll offset to the displayable range, so
    /// repeated scrolling can never bank invisible debt.
    pub fn set_chat_geometry(&mut self, total_rows: usize, viewport_rows: usize) {
        self.chat_total_rows = total_rows;
        self.chat_viewport_rows = viewport_rows;
        self.scroll_rows = self.scroll_rows.min(self.max_chat_scroll());
    }

    /// Highest scroll offset that still shows content: at the limit the
    /// earliest row sits one row below the viewport top, with the rest
    /// packed under it. Zero when everything fits.
    pub fn max_chat_scroll(&self) -> usize {
        max_chat_scroll(self.chat_total_rows, self.chat_viewport_rows)
    }

    /// Whether the view is pinned to the bottom.
    pub fn follows(&self) -> bool {
        self.scroll_rows == 0
    }

    /// Applies one message. Returns `true` when the UI should redraw.
    pub fn update(&mut self, msg: AppMsg) -> bool {
        self.reduce(msg).redraw
    }

    /// True once the user asked to quit.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Applies one message and returns its side effects for the runtime
    /// (turn spawning is the runtime's job; `App` is pure state).
    pub fn reduce(&mut self, msg: AppMsg) -> Effect {
        match msg {
            AppMsg::Key(key) => self.on_key(key),
            AppMsg::Assistant(text) => {
                self.push(ChatItem::new(text, ItemKind::Assistant));
                Effect::redraw()
            }
            AppMsg::AssistantDelta(delta) => {
                // Streaming append: slices extend the in-progress reply so
                // the transcript holds one item per reply, not per slice.
                // Scroll position is untouched, so a pinned tail follows
                // while a scrolled-up view stays put.
                match self.items.last_mut() {
                    Some(item) if item.kind == ItemKind::Assistant => item.text.push_str(&delta),
                    _ => self.push(ChatItem::new(delta, ItemKind::Assistant)),
                }
                Effect::redraw()
            }
            AppMsg::ToolStarted(call) => {
                self.push(ChatItem::new(
                    format!("→ {} ({})", call.name, call.arguments),
                    ItemKind::ToolStarted,
                ));
                Effect::redraw()
            }
            AppMsg::ToolResult(call, Ok(result)) => {
                self.push(ChatItem::new(
                    format!("← {} ok: {}", call.name, truncate(&result, 200)),
                    ItemKind::ToolOk,
                ));
                Effect::redraw()
            }
            AppMsg::ToolResult(call, Err(err)) => {
                self.push(ChatItem::new(
                    format!("← {} failed: {}", call.name, err.message),
                    ItemKind::ToolErr,
                ));
                Effect::redraw()
            }
            AppMsg::Completed(n) => {
                self.push(ChatItem::new(
                    format!("· done ({n} iteration{})", if n == 1 { "" } else { "s" }),
                    ItemKind::Notice,
                ));
                Effect::redraw()
            }
            AppMsg::Failed(err) => {
                self.push(ChatItem::new(format!("✗ {err}"), ItemKind::Error));
                Effect::redraw()
            }
            AppMsg::TurnFinished => {
                self.turn_finished();
                Effect::redraw()
            }
            AppMsg::Notice(text) => {
                self.push(ChatItem::new(text, ItemKind::Notice));
                Effect::redraw()
            }
            AppMsg::ScrollUp => {
                self.scroll_chat(WHEEL_ROWS as isize);
                Effect::redraw()
            }
            AppMsg::ScrollDown => {
                self.scroll_chat(-(WHEEL_ROWS as isize));
                Effect::redraw()
            }
        }
    }

    /// Called by the runtime once a turn task is actually spawned for a
    /// submitted message, to show a busy indicator while it streams.
    pub fn turn_started(&mut self) {
        self.live_turns += 1;
    }

    /// Called by the runtime when a turn stream ends, successfully or not.
    /// `AppMsg::Completed`/`Failed` already carry the outcome; this only
    /// clears the busy state. Once no turn is live the anchor has served
    /// its purpose, so it disarms (a later submit re-arms it).
    pub fn turn_finished(&mut self) {
        self.live_turns = self.live_turns.saturating_sub(1);
        if self.live_turns == 0 {
            self.anchor_follow = false;
        }
    }

    /// Handles a keypress, returning the effect for the runtime. Every
    /// key that touches editor or scroll state needs a redraw; only a
    /// submitted chat message carries `submitted`.
    fn on_key(&mut self, key: KeyEvent) -> Effect {
        match key {
            KeyEvent::Char(ch) => {
                self.editor.insert(ch);
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::Enter => self.submit(),
            KeyEvent::Newline => {
                self.editor.insert('\n');
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::Backspace => {
                self.editor.backspace();
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::Delete => {
                self.editor.delete();
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::Left => {
                self.editor.left();
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::Right => {
                self.editor.right();
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::Up => {
                self.editor.move_up(self.input_width);
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::Down => {
                self.editor.move_down(self.input_width);
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::Home => {
                self.editor.home(self.input_width);
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::End => {
                self.editor.end(self.input_width);
                self.follow_input_cursor();
                Effect::redraw()
            }
            KeyEvent::PageUp => {
                self.scroll_page_up();
                Effect::redraw()
            }
            KeyEvent::PageDown => {
                self.scroll_page_down();
                Effect::redraw()
            }
            // Ctrl+C interrupts the app; Esc alone is a no-op (quitting
            // is `/quit` only).
            KeyEvent::Interrupt => {
                self.quit = true;
                Effect::redraw()
            }
            KeyEvent::Esc => Effect::default(),
        }
    }

    fn submit(&mut self) -> Effect {
        if self.editor.is_empty() {
            return Effect::default();
        }
        let line = self.editor.take();
        self.input_scroll = 0;
        let trimmed = line.trim().to_owned();
        if trimmed.is_empty() {
            // Whitespace-only submit: still clear the editor.
            return Effect::redraw();
        }
        match trimmed.as_str() {
            "/quit" | "/exit" => {
                self.quit = true;
                return Effect::redraw();
            }
            "/clear" => {
                self.items.clear();
                self.scroll_rows = 0;
                self.anchor_follow = false;
                return Effect::redraw();
            }
            cmd if cmd.starts_with('/') => {
                self.push(ChatItem::new(
                    format!("unknown command: {cmd}"),
                    ItemKind::Error,
                ));
                return Effect::redraw();
            }
            _ => {}
        }
        self.push(ChatItem::new(trimmed.clone(), ItemKind::User));
        self.anchor_follow = true;
        Effect {
            redraw: true,
            submitted: Some(trimmed),
        }
    }

    fn push(&mut self, item: ChatItem) {
        // Row-based scroll needs no adjustment here: rows hidden below
        // the viewport stay hidden as new items stream in, and zero
        // stays pinned to the newest content either way.
        self.items.push(item);
    }

    fn scroll_page_up(&mut self) {
        self.scroll_chat(PAGE_ROWS as isize);
    }

    fn scroll_page_down(&mut self) {
        self.scroll_chat(-(PAGE_ROWS as isize));
    }

    /// Moves the chat viewport `delta` content rows (`+` scrolls up
    /// toward older content, `-` scrolls down). Reaching zero
    /// re-engages follow mode; the view clamps against the measured
    /// content height at draw time. Any manual scroll disarms the
    /// streaming follow anchor; every scroll input funnels through here.
    fn scroll_chat(&mut self, delta: isize) {
        self.anchor_follow = false;
        self.scroll_rows = self.scroll_rows.saturating_add_signed(delta);
    }

    /// Highest first-visible input row: the box shows at most
    /// `INPUT_VISIBLE_ROWS` rows.
    fn max_input_scroll(&self) -> usize {
        self.input_rows().saturating_sub(INPUT_VISIBLE_ROWS)
    }

    fn clamp_input_scroll(&mut self) {
        self.input_scroll = self.input_scroll.min(self.max_input_scroll());
    }

    /// Keeps the input cursor inside the visible input window after an
    /// edit, resize, or cursor move.
    fn follow_input_cursor(&mut self) {
        let (row, _) = self.editor.cursor_visual(self.input_width);
        if row < self.input_scroll {
            self.input_scroll = row;
        } else if row >= self.input_scroll + INPUT_VISIBLE_ROWS {
            self.input_scroll = row + 1 - INPUT_VISIBLE_ROWS;
        }
        self.clamp_input_scroll();
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: KeyEvent) -> AppMsg {
        AppMsg::Key(k)
    }

    fn last(app: &App) -> &ChatItem {
        app.items().last().expect("item pushed")
    }

    #[test]
    fn typing_builds_input() {
        let mut app = App::new();
        for c in "hello".chars() {
            assert!(app.update(key(KeyEvent::Char(c))));
        }
        assert_eq!(app.input(), "hello");
        assert_eq!(app.input_cursor(), 5);
    }

    #[test]
    fn enter_appends_user_item_and_clears_input() {
        let mut app = App::new();
        for c in "hi there".chars() {
            app.update(key(KeyEvent::Char(c)));
        }
        let eff = app.reduce(key(KeyEvent::Enter));
        assert!(eff.redraw);
        assert_eq!(eff.submitted.as_deref(), Some("hi there"));
        assert_eq!(app.items().len(), 1);
        assert_eq!(last(&app).kind, ItemKind::User);
        assert_eq!(last(&app).text, "hi there");
        assert_eq!(app.input(), "");
    }

    #[test]
    fn empty_enter_is_ignored() {
        let mut app = App::new();
        assert!(!app.update(key(KeyEvent::Enter)));
        assert!(app.items().is_empty());
    }

    #[test]
    fn whitespace_only_enter_clears_input_without_items() {
        let mut app = App::new();
        app.update(key(KeyEvent::Char(' ')));
        // Redraw (editor cleared) but nothing appended.
        assert!(app.update(key(KeyEvent::Enter)));
        assert!(app.items().is_empty());
        assert_eq!(app.input(), "");
        assert_eq!(app.live_turns, 0);
    }

    #[test]
    fn quit_command_sets_flag() {
        let mut app = App::new();
        for c in "/quit".chars() {
            app.update(key(KeyEvent::Char(c)));
        }
        app.update(key(KeyEvent::Enter));
        assert!(app.should_quit());

        let mut app = App::new();
        for c in "/exit".chars() {
            app.update(key(KeyEvent::Char(c)));
        }
        app.update(key(KeyEvent::Enter));
        assert!(app.should_quit());
    }

    #[test]
    fn clear_command_empties_transcript() {
        let mut app = App::new();
        app.update(AppMsg::Assistant("x".into()));
        app.update(AppMsg::Failed("y".into()));
        assert_eq!(app.items().len(), 2);
        for c in "/clear".chars() {
            app.update(key(KeyEvent::Char(c)));
        }
        assert!(app.update(key(KeyEvent::Enter)));
        assert!(app.items().is_empty());
        assert_eq!(app.scroll_rows(), 0);
        assert!(app.follows());
        assert!(!app.should_quit());
    }

    #[test]
    fn unknown_command_is_an_inline_error() {
        let mut app = App::new();
        for c in "/bogus".chars() {
            app.update(key(KeyEvent::Char(c)));
        }
        assert!(app.update(key(KeyEvent::Enter)));
        assert_eq!(last(&app).kind, ItemKind::Error);
        assert_eq!(last(&app).text, "unknown command: /bogus");
        assert!(!app.should_quit());
    }

    #[test]
    fn turn_events_append_items() {
        let mut app = App::new();
        app.turn_started();

        assert!(app.update(AppMsg::Assistant("answer".into())));
        assert_eq!(last(&app).kind, ItemKind::Assistant);

        let call = ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"x\"}".into(),
        };
        assert!(app.update(AppMsg::ToolStarted(call.clone())));
        assert_eq!(last(&app).text, "→ read_file ({\"path\":\"x\"})");
        assert_eq!(last(&app).kind, ItemKind::ToolStarted);

        assert!(app.update(AppMsg::ToolResult(
            call.clone(),
            Ok("file contents here".into())
        )));
        assert_eq!(last(&app).text, "← read_file ok: file contents here");
        assert_eq!(last(&app).kind, ItemKind::ToolOk);

        assert!(app.update(AppMsg::ToolResult(
            call,
            Err(ToolError {
                tool: "read_file".into(),
                message: "boom".into()
            })
        )));
        assert_eq!(last(&app).text, "← read_file failed: boom");
        assert_eq!(last(&app).kind, ItemKind::ToolErr);

        assert!(app.update(AppMsg::Completed(1)));
        assert_eq!(last(&app).text, "· done (1 iteration)");
        assert_eq!(last(&app).kind, ItemKind::Notice);

        assert!(app.update(AppMsg::TurnFinished));
        assert_eq!(app.live_turns, 0);
    }

    #[test]
    fn failed_turn_appends_error_and_releases_live_slot() {
        let mut app = App::new();
        app.turn_started();
        app.update(AppMsg::Failed("boom".into()));
        assert!(app.update(AppMsg::TurnFinished));
        assert_eq!(last(&app).kind, ItemKind::Error);
        assert_eq!(app.live_turns, 0);
    }

    #[test]
    fn live_turns_never_underflow() {
        let mut app = App::new();
        app.update(AppMsg::TurnFinished);
        app.update(AppMsg::TurnFinished);
        assert_eq!(app.live_turns, 0);
    }

    #[test]
    fn page_up_breaks_follow_and_page_down_restores() {
        let mut app = App::new();
        for i in 0..25 {
            app.update(AppMsg::Assistant(format!("m{i}")));
        }
        assert!(app.follows());
        assert_eq!(app.scroll_rows(), 0);

        assert!(app.update(key(KeyEvent::PageUp)));
        assert!(!app.follows());
        assert_eq!(app.scroll_rows(), PAGE_ROWS);

        assert!(app.update(key(KeyEvent::PageDown)));
        assert_eq!(app.scroll_rows(), 0);
        assert!(app.follows());

        // PageDown at the bottom stays pinned to the newest item.
        assert!(app.update(key(KeyEvent::PageDown)));
        assert!(app.follows());
        assert_eq!(app.scroll_rows(), 0);
    }

    #[test]
    fn page_up_at_top_stays_pinned_but_marks_scrolled() {
        let mut app = App::new();
        app.update(AppMsg::Assistant("only".into()));
        assert!(app.update(key(KeyEvent::PageUp)));
        assert!(!app.follows());
        assert_eq!(app.scroll_rows(), PAGE_ROWS);
    }

    #[test]
    fn scroll_rows_survive_new_items() {
        let mut app = App::new();
        for i in 0..3 {
            app.update(AppMsg::Assistant(format!("m{i}")));
        }
        assert!(app.update(key(KeyEvent::PageUp)));
        assert!(!app.follows());
        let pinned = app.scroll_rows();

        // New items stream in below; the hidden-row count is untouched.
        app.update(AppMsg::Assistant("new".into()));
        assert!(!app.follows());
        assert_eq!(app.scroll_rows(), pinned);
    }

    #[test]
    fn esc_is_ignored() {
        let mut app = App::new();
        assert!(!app.update(key(KeyEvent::Esc)));
        assert!(!app.should_quit());
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new();
        assert!(app.update(key(KeyEvent::Interrupt)));
        assert!(app.should_quit());
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.update(key(KeyEvent::Char(ch)));
        }
    }

    #[test]
    fn newline_key_inserts_line_without_submitting() {
        let mut app = App::new();
        type_text(&mut app, "ab");
        let effect = app.reduce(key(KeyEvent::Newline));
        assert!(effect.redraw);
        assert!(effect.submitted.is_none());
        type_text(&mut app, "cd");
        assert_eq!(app.input(), "ab\ncd");
        let effect = app.reduce(key(KeyEvent::Enter));
        assert_eq!(effect.submitted.as_deref(), Some("ab\ncd"));
        assert_eq!(app.input(), "");
        assert_eq!(app.input_scroll(), 0);
    }

    #[test]
    fn up_down_move_cursor_and_follow_with_scroll() {
        let mut app = App::new();
        app.set_input_width(10);
        // Ten "aa" words wrap to 4 rows at width 10.
        type_text(&mut app, &"aa ".repeat(10));
        assert_eq!(app.input_rows(), 4);
        // Cursor starts on the last row; the window shows the tail.
        assert_eq!(app.input_cursor_visual(), (3, 3));
        assert_eq!(app.input_scroll(), 1);

        app.update(key(KeyEvent::Up));
        assert_eq!(app.input_cursor_visual(), (2, 3));
        assert_eq!(app.input_scroll(), 1);
        app.update(key(KeyEvent::Up));
        assert_eq!(app.input_cursor_visual(), (1, 3));
        assert_eq!(app.input_scroll(), 1);
        // Cursor reaches the hidden head row: the window scrolls up.
        app.update(key(KeyEvent::Up));
        assert_eq!(app.input_cursor_visual(), (0, 3));
        assert_eq!(app.input_scroll(), 0);

        // Up on the first row is a no-op for the cursor.
        app.update(key(KeyEvent::Up));
        assert_eq!(app.input_cursor_visual(), (0, 3));

        app.update(key(KeyEvent::Down));
        app.update(key(KeyEvent::Down));
        assert_eq!(app.input_cursor_visual(), (2, 3));
    }

    #[test]
    fn typing_at_end_scrolls_tall_input() {
        let mut app = App::new();
        app.set_input_width(10);
        type_text(&mut app, &"aa ".repeat(10));
        // Cursor on row 3 of 4 keeps rows 1..=3 visible.
        assert_eq!(app.input_scroll(), 1);
    }

    #[test]
    fn input_width_resize_reclamps_scroll() {
        let mut app = App::new();
        app.set_input_width(10);
        type_text(&mut app, &"aa ".repeat(10));
        assert_eq!(app.input_scroll(), 1);
        // Widening collapses to one row: scroll pins back to 0.
        app.set_input_width(80);
        assert_eq!(app.input_rows(), 1);
        assert_eq!(app.input_scroll(), 0);
    }

    #[test]
    fn wheel_scrolls_chat_one_row_per_notch() {
        let mut app = App::new();
        for i in 0..10 {
            app.update(AppMsg::Assistant(format!("m{i}")));
        }
        assert!(app.update(AppMsg::ScrollUp));
        assert!(!app.follows());
        assert_eq!(app.scroll_rows(), 1);
        assert!(app.update(AppMsg::ScrollUp));
        assert_eq!(app.scroll_rows(), 2);
        assert!(app.update(AppMsg::ScrollDown));
        assert_eq!(app.scroll_rows(), 1);
        // Scrolling down past the tail re-engages follow.
        assert!(app.update(AppMsg::ScrollDown));
        assert!(app.update(AppMsg::ScrollDown));
        assert!(app.follows());
        assert_eq!(app.scroll_rows(), 0);
    }

    #[test]
    fn max_chat_scroll_stops_earliest_row_one_below_top() {
        // 30 rows in a 9-row viewport: earliest row may sit no lower
        // than viewport row 1, i.e. 22 rows hidden below.
        assert_eq!(max_chat_scroll(30, 9), 22);
        assert_eq!(max_chat_scroll(10, 9), 2);
        assert_eq!(max_chat_scroll(9, 9), 0);
        assert_eq!(max_chat_scroll(0, 9), 0);
    }

    #[test]
    fn chat_geometry_clamps_banked_scroll_debt() {
        let mut app = App::new();
        for i in 0..10 {
            app.update(AppMsg::Assistant(format!("m{i}")));
        }
        // Ten rows of content: five pages up banks 50 rows.
        for _ in 0..5 {
            app.update(key(KeyEvent::PageUp));
        }
        assert_eq!(app.scroll_rows(), 50);
        // Reporting the measured geometry pulls it back to the limit
        // (10 rows, 9-row viewport -> earliest row at viewport row 1).
        app.set_chat_geometry(10, 9);
        assert_eq!(app.scroll_rows(), 2);
        assert!(!app.follows());
        // Content that fits the viewport always re-engages follow.
        app.set_chat_geometry(5, 9);
        assert_eq!(app.scroll_rows(), 0);
        assert!(app.follows());
    }

    #[test]
    fn notice_appends_system_line() {
        let mut app = App::new();
        assert!(app.update(AppMsg::Notice("model → x".into())));
        assert_eq!(last(&app).kind, ItemKind::Notice);
        assert_eq!(last(&app).text, "model → x");
    }

    #[test]
    fn assistant_deltas_append_to_one_item() {
        let mut app = App::new();
        assert!(app.update(AppMsg::AssistantDelta("hel".into())));
        assert!(app.update(AppMsg::AssistantDelta("lo".into())));
        assert_eq!(app.items().len(), 1);
        assert_eq!(last(&app).kind, ItemKind::Assistant);
        assert_eq!(last(&app).text, "hello");
    }

    #[test]
    fn assistant_delta_starts_new_item_after_other_kinds() {
        let mut app = App::new();
        assert!(app.update(AppMsg::AssistantDelta("first".into())));
        assert!(app.update(AppMsg::Completed(1)));
        assert!(app.update(AppMsg::AssistantDelta("second".into())));
        assert_eq!(app.items().len(), 3);
        assert_eq!(app.items()[0].text, "first");
        assert_eq!(app.items()[2].text, "second");
        // A delta keeps extending only the trailing reply.
        assert!(app.update(AppMsg::AssistantDelta("!".into())));
        assert_eq!(app.items().len(), 3);
        assert_eq!(app.items()[2].text, "second!");
    }

    #[test]
    fn assistant_deltas_do_not_disturb_chat_scroll() {
        let mut app = App::new();
        for i in 0..3 {
            app.update(AppMsg::Assistant(format!("m{i}")));
        }
        assert!(app.update(key(KeyEvent::PageUp)));
        assert!(!app.follows());
        let pinned = app.scroll_rows();
        assert!(app.update(AppMsg::AssistantDelta("more".into())));
        assert_eq!(app.scroll_rows(), pinned);
        assert!(!app.follows());
    }

    fn submit_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.update(key(KeyEvent::Char(c)));
        }
        app.update(key(KeyEvent::Enter));
    }

    #[test]
    fn submit_arms_anchor_follow() {
        let mut app = App::new();
        assert!(!app.anchor_follow());
        submit_text(&mut app, "hi");
        assert!(app.anchor_follow());
    }

    #[test]
    fn non_user_submits_do_not_arm_anchor() {
        let mut app = App::new();
        // Empty and whitespace-only submits push nothing.
        assert!(!app.update(key(KeyEvent::Enter)));
        assert!(!app.anchor_follow());
        // Unknown commands and quit never produce a user message.
        submit_text(&mut app, "/bogus");
        assert!(!app.anchor_follow());
        assert!(!app.should_quit());
    }

    #[test]
    fn clear_disarms_anchor_follow() {
        let mut app = App::new();
        submit_text(&mut app, "hi");
        assert!(app.anchor_follow());
        submit_text(&mut app, "/clear");
        assert!(app.items().is_empty());
        assert!(!app.anchor_follow());
    }

    #[test]
    fn manual_scroll_disarms_anchor_follow() {
        let mut app = App::new();
        submit_text(&mut app, "hi");
        assert!(app.update(key(KeyEvent::PageUp)));
        assert!(!app.anchor_follow());

        submit_text(&mut app, "again");
        assert!(app.anchor_follow());
        assert!(app.update(AppMsg::ScrollUp));
        assert!(!app.anchor_follow());
    }

    #[test]
    fn turn_finish_disarms_anchor_only_when_no_turn_live() {
        let mut app = App::new();
        submit_text(&mut app, "hi");
        app.turn_started();
        app.turn_started();
        app.update(AppMsg::TurnFinished);
        assert!(app.anchor_follow(), "second turn still live");
        app.update(AppMsg::TurnFinished);
        assert!(!app.anchor_follow());
    }

    #[test]
    fn set_scroll_rows_clamps_to_displayable_range() {
        let mut app = App::new();
        app.set_chat_geometry(10, 9);
        app.set_scroll_rows(100);
        assert_eq!(app.scroll_rows(), 2);
        app.set_scroll_rows(1);
        assert_eq!(app.scroll_rows(), 1);
    }

    #[test]
    fn clear_input_discards_model_command() {
        let mut app = App::new();
        type_text(&mut app, "/model");
        assert_eq!(app.input(), "/model");
        app.clear_input();
        assert_eq!(app.input(), "");
        assert_eq!(app.input_scroll(), 0);
    }
}
