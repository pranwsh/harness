//! Shareable filter-popup behavior for autocomplete providers.
//!
//! This crate owns no domain knowledge: providers implement the small
//! [`FilterSource`] trait (what counts as a query, how to list matches,
//! what `Enter` means) and the generic [`FilterPopup`] driver supplies the
//! rest — open/refresh/close syncing from the input line plus
//! arrow/Tab/Enter/Esc handling that never steals the entry bar. Editing
//! keys always fall through to `App`, so typing keeps working while the
//! list filters. Future plugins get a searchable popup by implementing
//! the trait and wrapping the driver; the TUI shell only ever calls the
//! domain-free `sync` / `handle_key` / `close` / `is_active` surface.
//!
//! A plain library crate on purpose (no DI service): it is linked by
//! provider crates the way `tui-state` is, keeping the crate graph
//! acyclic and `tui-popup` pure state.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use harness_tui_popup::{ActivePopup, Popup};
use harness_tui_state::app::{App, KeyEvent};

/// Domain logic a filter popup needs. Required methods are the query
/// extraction, the match listing, and the `Enter` action; everything else
/// has a default so providers only override what differs.
pub trait FilterSource: Send + Sync {
    /// Popup title, with the surrounding spaces the border title expects.
    fn title(&self) -> &str;

    /// Extracts the filter query from the input line, or `None` when this
    /// provider owns nothing about the line (which must end its session).
    fn candidate(&self, input: &str) -> Option<String>;

    /// Lists matches for a query, in display order.
    fn items(&self, query: &str) -> Vec<String>;

    /// Applies the highlighted row. Returns `true` when consumed (the
    /// driver re-syncs afterwards); `false` falls through to the shell
    /// (e.g. an already-exact command left for normal execution).
    fn on_enter(&self, selected: &str, app: &mut App) -> bool;

    /// Active value marked with `●`, distinct from the arrow-key cursor.
    /// `None` shows no marker.
    fn current(&self) -> Option<String> {
        None
    }

    /// Initial cursor row when the list (re)opens. Defaults to the head;
    /// providers with an active value typically jump to it.
    fn initial_selected(&self, items: &[String]) -> usize {
        let _ = items;
        0
    }

    /// Input line `Tab` writes for a highlighted row. Defaults to the row
    /// itself; providers whose query is prefixed (e.g. `/model <id>`)
    /// rebuild the full line here.
    fn render_completion(&self, selected: &str) -> String {
        selected.to_owned()
    }

    /// Extra cleanup on `Esc` dismissal. Defaults to leaving the input
    /// untouched; providers that staged a trigger line typically clear it.
    fn on_dismiss(&self, _app: &mut App) {}
}

/// Generic non-modal filter popup: one owned [`Popup`] surface plus a
/// session flag. At most the flag outlives a visibly empty list, so typing
/// past zero matches reopens as soon as anything matches again.
pub struct FilterPopup<S> {
    popup: Arc<Popup>,
    source: S,
    active: AtomicBool,
}

impl<S: FilterSource> FilterPopup<S> {
    pub fn new(source: S) -> Self {
        FilterPopup {
            popup: Arc::new(Popup::new()),
            source,
            active: AtomicBool::new(false),
        }
    }

    /// Whether a filter session is live (independent of list visibility:
    /// stays true across zero-match stretches until dismissed or edited
    /// away). The shell uses this — not `is_open` — to decide which
    /// provider owns the input line.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub fn is_open(&self) -> bool {
        self.popup.is_open()
    }

    pub fn snapshot(&self) -> Option<ActivePopup> {
        self.popup.snapshot()
    }

    /// Ends the session and hides the list. The shell calls this to
    /// explicitly hand the line from one provider to another.
    pub fn close(&self) {
        self.end();
    }

    /// Surface handle for background refresh tasks landing after the
    /// session moved on. [`Popup::refresh_items`] is a no-op when closed,
    /// so late landings can never reopen anything.
    pub fn popup(&self) -> Arc<Popup> {
        self.popup.clone()
    }

    fn end(&self) {
        self.active.store(false, Ordering::SeqCst);
        self.popup.close();
    }

    /// Reconciles the session with the input line: no candidate ends it;
    /// an empty match list hides the rows but stays active; otherwise the
    /// list opens (cursor from [`FilterSource::initial_selected`]) or
    /// refreshes with the cursor kept on the same value when possible.
    pub fn sync(&self, app: &App) {
        let Some(query) = self.source.candidate(app.input()) else {
            self.end();
            return;
        };
        self.active.store(true, Ordering::SeqCst);
        let items = self.source.items(&query);
        if items.is_empty() {
            if self.popup.is_open() {
                self.popup.close();
            }
            return;
        }
        if self.popup.is_open() {
            self.popup.refresh_items(items, self.source.current());
        } else {
            let selected = self.source.initial_selected(&items);
            self.popup
                .open(self.source.title(), items, selected, self.source.current());
        }
    }

    /// Handles one key. Returns `true` when consumed (never reaches
    /// `App`), `false` to fall through:
    /// - `Up`/`Down`: move cursor, consume.
    /// - `Esc`: end session, run [`FilterSource::on_dismiss`], consume.
    /// - `Tab`: fill [`FilterSource::render_completion`] into the input
    ///   and re-filter, consume.
    /// - `Enter`: delegate to [`FilterSource::on_enter`] (re-syncing when
    ///   consumed); no highlighted row falls through.
    /// - `Interrupt` and anything else: fall through so editing stays in
    ///   `App` and re-filters via the shell's post-reduce `sync`.
    pub fn handle_key(&self, key: KeyEvent, app: &mut App) -> bool {
        if !self.popup.is_open() {
            return false;
        }
        match key {
            KeyEvent::Up => {
                self.popup.move_up();
                true
            }
            KeyEvent::Down => {
                self.popup.move_down();
                true
            }
            KeyEvent::Esc => {
                self.end();
                self.source.on_dismiss(app);
                true
            }
            KeyEvent::Tab => {
                if let Some(item) = self.popup.selected_item() {
                    app.set_input(&self.source.render_completion(&item));
                    self.sync(app);
                }
                true
            }
            KeyEvent::Enter => match self.popup.selected_item() {
                Some(selected) => {
                    let consumed = self.source.on_enter(&selected, app);
                    if consumed {
                        self.sync(app);
                    }
                    consumed
                }
                None => false,
            },
            KeyEvent::Interrupt => false,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_tui_state::app::AppMsg;

    /// Mirrors the slash-command provider: `/`-prefix candidate,
    /// prefix matches, complete-unless-exact `Enter`.
    struct FakeSource;

    impl FilterSource for FakeSource {
        fn title(&self) -> &str {
            " fake "
        }

        fn candidate(&self, input: &str) -> Option<String> {
            input
                .strip_prefix('/')
                .and_then(|rest| (!rest.contains(char::is_whitespace)).then(|| rest.to_owned()))
        }

        fn items(&self, query: &str) -> Vec<String> {
            ["/clear", "/exit", "/model", "/quit"]
                .iter()
                .filter(|cmd| cmd.strip_prefix('/').unwrap_or_default().starts_with(query))
                .map(|cmd| cmd.to_string())
                .collect()
        }

        fn on_enter(&self, selected: &str, app: &mut App) -> bool {
            if app.input().trim() != selected {
                app.set_input(selected);
                true
            } else {
                false
            }
        }
    }

    fn popup() -> FilterPopup<FakeSource> {
        FilterPopup::new(FakeSource)
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
    }

    #[test]
    fn sync_opens_refreshes_and_closes() {
        let filter = popup();
        let mut app = App::new();
        assert!(!filter.is_active());

        type_text(&mut app, "/");
        filter.sync(&app);
        assert!(filter.is_active());
        assert!(filter.is_open());
        assert_eq!(filter.snapshot().expect("open").items.len(), 4);

        type_text(&mut app, "c");
        filter.sync(&app);
        assert_eq!(
            filter.snapshot().expect("open").items,
            vec!["/clear".to_owned()]
        );

        app.set_input("hello");
        filter.sync(&app);
        assert!(!filter.is_active());
        assert!(!filter.is_open());
    }

    #[test]
    fn zero_matches_hides_but_stays_active_until_edited_away() {
        let filter = popup();
        let mut app = App::new();
        type_text(&mut app, "/bogus");
        filter.sync(&app);
        assert!(filter.is_active());
        assert!(!filter.is_open());

        // Typing back into a match reopens without a new session.
        app.set_input("/c");
        filter.sync(&app);
        assert!(filter.is_open());
        assert_eq!(
            filter.snapshot().expect("open").items,
            vec!["/clear".to_owned()]
        );
    }

    #[test]
    fn arrows_move_and_editing_falls_through() {
        let filter = popup();
        let mut app = App::new();
        type_text(&mut app, "/");
        filter.sync(&app);
        assert!(filter.handle_key(KeyEvent::Down, &mut app));
        assert_eq!(filter.snapshot().expect("open").selected, 1);
        assert!(!filter.handle_key(KeyEvent::Char('x'), &mut app));
        assert!(!filter.handle_key(KeyEvent::Backspace, &mut app));
        assert!(!filter.handle_key(KeyEvent::Interrupt, &mut app));
        assert_eq!(app.input(), "/");
    }

    #[test]
    fn tab_and_enter_complete_then_fall_through_when_exact() {
        let filter = popup();
        let mut app = App::new();
        type_text(&mut app, "/c");
        filter.sync(&app);
        assert!(filter.handle_key(KeyEvent::Tab, &mut app));
        assert_eq!(app.input(), "/clear");

        let mut app = App::new();
        type_text(&mut app, "/c");
        filter.sync(&app);
        assert!(filter.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(app.input(), "/clear");
        assert!(!filter.handle_key(KeyEvent::Enter, &mut app));
    }

    #[test]
    fn esc_ends_session_and_leaves_input_by_default() {
        let filter = popup();
        let mut app = App::new();
        type_text(&mut app, "/c");
        filter.sync(&app);
        assert!(filter.handle_key(KeyEvent::Esc, &mut app));
        assert!(!filter.is_active());
        assert!(!filter.is_open());
        assert_eq!(app.input(), "/c");
    }

    #[test]
    fn handle_key_when_closed_falls_through() {
        let filter = popup();
        let mut app = App::new();
        assert!(!filter.handle_key(KeyEvent::Up, &mut app));
        assert!(!filter.handle_key(KeyEvent::Tab, &mut app));
        assert!(!filter.handle_key(KeyEvent::Enter, &mut app));
    }

    #[test]
    fn close_ends_session_explicitly() {
        let filter = popup();
        let mut app = App::new();
        type_text(&mut app, "/");
        filter.sync(&app);
        assert!(filter.is_active());
        filter.close();
        assert!(!filter.is_active());
        assert!(!filter.is_open());
    }
}
