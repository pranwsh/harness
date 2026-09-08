//! Generic floating list popup: single-select state only.
//!
//! Fully decoupled by design: this crate knows nothing about models,
//! autocomplete, or any other domain. Providers push plain `String` items
//! (plus a title and which item is "current"); provider plugins such as
//! `tui-model` own the bridge that fills those in, while the TUI shell
//! renders the snapshot above the input bar.
//!
//! Provided as `Arc<Popup>` under [`KEY_POPUP`](harness_contracts::KEY_POPUP).
//! All methods lock briefly and clone, so `draw` never holds the lock and
//! background refreshes can update items without stalling input.

use std::sync::Mutex;

use harness_contracts::KEY_POPUP;
use harness_core::{Context, Result};

/// Cloned view of an open popup, safe to render without holding any lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePopup {
    pub title: String,
    pub items: Vec<String>,
    /// Cursor row the arrow keys move.
    pub selected: usize,
    /// Active value (e.g. current model), marked distinctly from the cursor.
    pub current: Option<String>,
}

#[derive(Debug, Default)]
struct Inner {
    open: bool,
    title: String,
    items: Vec<String>,
    selected: usize,
    current: Option<String>,
}

/// Single floating list. At most one is visible at a time; opening replaces
/// any previous popup.
#[derive(Debug, Default)]
pub struct Popup {
    state: Mutex<Inner>,
}

impl Popup {
    pub fn new() -> Self {
        Popup {
            state: Mutex::new(Inner::default()),
        }
    }

    /// Opens (or replaces) the popup. The cursor is clamped into range;
    /// an empty item list still opens so callers can show a placeholder row.
    pub fn open(
        &self,
        title: impl Into<String>,
        items: Vec<String>,
        selected: usize,
        current: Option<String>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.open = true;
        state.title = title.into();
        state.items = items;
        state.selected = selected.min(state.items.len().saturating_sub(1));
        state.current = current;
    }

    /// Closes the popup and clears its contents.
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *state = Inner::default();
    }

    pub fn is_open(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).open
    }

    /// Cloned snapshot for rendering; `None` when closed.
    pub fn snapshot(&self) -> Option<ActivePopup> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open {
            return None;
        }
        Some(ActivePopup {
            title: state.title.clone(),
            items: state.items.clone(),
            selected: state.selected.min(state.items.len().saturating_sub(1)),
            current: state.current.clone(),
        })
    }

    /// Currently highlighted item, if open and non-empty.
    pub fn selected_item(&self) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open {
            return None;
        }
        state.items.get(state.selected).cloned()
    }

    pub fn move_up(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open || state.items.is_empty() {
            return;
        }
        state.selected = state
            .selected
            .checked_sub(1)
            .unwrap_or(state.items.len() - 1);
    }

    pub fn move_down(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open || state.items.is_empty() {
            return;
        }
        state.selected = (state.selected + 1) % state.items.len();
    }

    /// Replaces items while open (e.g. a background `/models` refresh
    /// landing after the popup opened). Keeps the cursor on the same value
    /// when it still exists, otherwise clamps. No-op when closed.
    pub fn refresh_items(&self, items: Vec<String>, current: Option<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open {
            return;
        }
        let keep = state.items.get(state.selected).cloned();
        state.items = items;
        state.current = current;
        state.selected = keep
            .and_then(|prev| state.items.iter().position(|item| *item == prev))
            .unwrap_or(0)
            .min(state.items.len().saturating_sub(1));
    }
}

/// Plugin providing the shared popup as `Arc<Popup>` under
/// [`KEY_POPUP`](harness_contracts::KEY_POPUP).
pub struct TuiPopupPlugin;

impl harness_core::Plugin for TuiPopupPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui-popup").provides(KEY_POPUP)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        ctx.provide_key(KEY_POPUP, std::sync::Arc::new(Popup::new()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn popup() -> Popup {
        Popup::new()
    }

    #[test]
    fn closed_by_default() {
        let p = popup();
        assert!(!p.is_open());
        assert_eq!(p.snapshot(), None);
        assert_eq!(p.selected_item(), None);
    }

    #[test]
    fn open_clamps_selection() {
        let p = popup();
        p.open("t", vec!["a".into(), "b".into()], 99, None);
        assert!(p.is_open());
        let snap = p.snapshot().expect("open");
        assert_eq!(snap.selected, 1);
        assert_eq!(p.selected_item().as_deref(), Some("b"));
    }

    #[test]
    fn arrows_wrap_around() {
        let p = popup();
        p.open("t", vec!["a".into(), "b".into()], 0, None);
        p.move_up();
        assert_eq!(p.snapshot().expect("open").selected, 1);
        p.move_down();
        assert_eq!(p.snapshot().expect("open").selected, 0);
    }

    #[test]
    fn refresh_keeps_cursor_on_same_value() {
        let p = popup();
        p.open("t", vec!["a".into(), "b".into()], 1, Some("a".into()));
        p.refresh_items(vec!["a".into(), "b".into(), "c".into()], Some("a".into()));
        assert_eq!(p.snapshot().expect("open").selected, 1);
        // Removed value falls back to the head.
        p.refresh_items(vec!["c".into()], Some("c".into()));
        assert_eq!(p.snapshot().expect("open").selected, 0);
    }

    #[test]
    fn refresh_when_closed_is_noop() {
        let p = popup();
        p.refresh_items(vec!["a".into()], None);
        assert!(!p.is_open());
    }

    #[test]
    fn close_clears() {
        let p = popup();
        p.open("t", vec!["a".into()], 0, None);
        p.close();
        assert!(!p.is_open());
        assert_eq!(p.snapshot(), None);
    }

    #[test]
    fn plugin_provides_popup_through_di() {
        let ctx = Context::root();
        ctx.load(TuiPopupPlugin).unwrap();
        let popup: std::sync::Arc<Popup> = ctx.inject_key(KEY_POPUP).unwrap();
        assert!(!popup.is_open());
    }
}
