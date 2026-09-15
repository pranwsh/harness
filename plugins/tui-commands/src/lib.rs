//! Slash-command autocomplete: the `/`-prefix bridge between a filter
//! popup and the static command vocabulary.
//!
//! Fully decoupled by design: this crate knows nothing about models,
//! config, agents, or execution — and shares no popup state with any
//! other provider. Filtering behavior is the shared
//! [`FilterPopup`](harness_tui_filter::FilterPopup) driver; this crate
//! only supplies the command source (candidate, prefix matches,
//! complete-unless-exact `Enter`). The TUI shell draws at most one
//! provider snapshot (model first, else commands), keeps the terminal
//! cursor in the input box, and routes keys slash-first; execution always
//! falls through to the existing paths (unknown-command error in
//! `App::submit`, model search in `ModelPopup`).
//!
//! Provided as `Arc<CommandPopup>` under
//! [`KEY_COMMAND_POPUP`](harness_contracts::KEY_COMMAND_POPUP). Injects
//! nothing.

use std::sync::Arc;

use harness_contracts::{
    KEY_COMMAND_POPUP,
    commands::{COMMANDS_TITLE, filter_commands, slash_candidate},
};
use harness_core::{Context, Result};
use harness_tui_filter::{FilterPopup, FilterSource};
use harness_tui_popup::ActivePopup;
use harness_tui_state::app::{App, KeyEvent};

/// Command source for the shared driver: `/`-prefix candidate and prefix
/// matches over [`ALL_COMMANDS`](harness_contracts::commands::ALL_COMMANDS).
/// `Enter` is the shared complete-unless-exact default (exact falls
/// through to execution).
struct CommandSource;

impl FilterSource for CommandSource {
    fn title(&self) -> &str {
        COMMANDS_TITLE
    }

    fn candidate(&self, input: &str) -> Option<String> {
        slash_candidate(input).map(str::to_owned)
    }

    fn items(&self, query: &str) -> Vec<String> {
        filter_commands(query)
    }
}

/// Autocomplete bridge: filters the static command list into its own
/// generic popup. Never clears or submits input itself — completion only
/// ever calls the generic [`App::set_input`], so the cursor stays in the
/// entry bar and execution stays with `App`/model.
pub struct CommandPopup {
    filter: FilterPopup<CommandSource>,
}

impl Default for CommandPopup {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPopup {
    pub fn new() -> Self {
        CommandPopup {
            filter: FilterPopup::new(CommandSource),
        }
    }

    pub fn is_active(&self) -> bool {
        self.filter.is_active()
    }

    pub fn is_open(&self) -> bool {
        self.filter.is_open()
    }

    pub fn snapshot(&self) -> Option<ActivePopup> {
        self.filter.snapshot()
    }

    /// Ends the session and hides the list. The shell calls this when
    /// handing the line to another provider (e.g. opening model search).
    pub fn close(&self) {
        self.filter.close();
    }

    /// Reconciles the session with the current input line: opens on a
    /// `/`-prefix, refreshes (selection kept by value) as the user narrows,
    /// closes on non-slash input, multi-token lines, or zero matches
    /// (e.g. `/bogus`, staying eligible to reopen).
    pub fn sync(&self, app: &App) {
        self.filter.sync(app);
    }

    /// Handles one key via the shared driver. Returns `true` when consumed
    /// (never reaches `App`), `false` to fall through so editing stays in
    /// `App` and re-filters via the runtime's post-reduce `sync`.
    pub fn handle_key(&self, key: KeyEvent, app: &mut App) -> bool {
        self.filter.handle_key(key, app)
    }
}

/// Plugin providing the slash-command bridge as `Arc<CommandPopup>` under
/// [`KEY_COMMAND_POPUP`](harness_contracts::KEY_COMMAND_POPUP).
/// Injects nothing: the bridge owns its popup surface, so it never parks
/// and can never observe or clobber another provider's list.
pub struct TuiCommandsPlugin;

impl harness_core::Plugin for TuiCommandsPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui-commands").provides(KEY_COMMAND_POPUP)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        ctx.provide_key(KEY_COMMAND_POPUP, Arc::new(CommandPopup::new()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_tui_popup::Popup;
    use harness_tui_state::app::AppMsg;

    fn popup() -> CommandPopup {
        CommandPopup::new()
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.update(AppMsg::Key(KeyEvent::Char(ch)));
        }
    }

    #[test]
    fn bare_slash_opens_with_all_commands() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/");
        bridge.sync(&app);
        assert!(bridge.is_open());
        let snap = bridge.snapshot().expect("open");
        assert_eq!(snap.items, vec!["/model".to_owned(), "/sessions".to_owned()]);
        assert_eq!(snap.selected, 0);
        assert_eq!(snap.current, None);
    }

    #[test]
    fn typing_narrows_and_backspacing_widens() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/");
        bridge.sync(&app);
        assert!(bridge.is_open());
        type_text(&mut app, "m");
        bridge.sync(&app);
        let snap = bridge.snapshot().expect("open");
        assert_eq!(snap.items, vec!["/model".to_owned()]);

        app.update(AppMsg::Key(KeyEvent::Backspace));
        bridge.sync(&app);
        assert_eq!(
            bridge.snapshot().expect("open").items,
            vec!["/model".to_owned(), "/sessions".to_owned()]
        );
    }

    #[test]
    fn no_match_or_non_slash_closes() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/bogus");
        bridge.sync(&app);
        assert!(!bridge.is_open());

        let mut app = App::new();
        type_text(&mut app, "/");
        bridge.sync(&app);
        assert!(bridge.is_open());
        type_text(&mut app, "hello");
        app.set_input("hello");
        bridge.sync(&app);
        assert!(!bridge.is_open());

        // Trailing args disqualify (`/model <query>` belongs to search,
        // which suppresses this list via the shell).
        let mut app = App::new();
        app.set_input("/model x");
        bridge.sync(&app);
        assert!(!bridge.is_open());
    }

    #[test]
    fn arrows_move_and_wrap() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/");
        bridge.sync(&app);
        // Two rows: moving steps through them and wraps around.
        assert!(bridge.handle_key(KeyEvent::Down, &mut app));
        assert_eq!(bridge.snapshot().expect("open").selected, 1);
        assert!(bridge.handle_key(KeyEvent::Down, &mut app));
        assert_eq!(bridge.snapshot().expect("open").selected, 0);
        assert!(bridge.handle_key(KeyEvent::Up, &mut app));
        assert_eq!(bridge.snapshot().expect("open").selected, 1);
        // Cursor never left the entry bar.
        assert_eq!(app.input(), "/");
    }

    #[test]
    fn tab_completes_selection_and_keeps_popup() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/m");
        bridge.sync(&app);
        assert!(bridge.handle_key(KeyEvent::Tab, &mut app));
        assert_eq!(app.input(), "/model");
        assert!(bridge.is_open());
        assert_eq!(
            bridge.snapshot().expect("open").items,
            vec!["/model".to_owned()]
        );
    }

    #[test]
    fn enter_completes_first_then_falls_through_when_exact() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/m");
        bridge.sync(&app);
        // First Enter: completes, consumes.
        assert!(bridge.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(app.input(), "/model");
        assert!(bridge.is_open());
        // Second Enter: already exact, falls through to execute.
        assert!(!bridge.handle_key(KeyEvent::Enter, &mut app));
    }

    #[test]
    fn enter_on_fully_typed_command_falls_through_immediately() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/model");
        bridge.sync(&app);
        assert!(bridge.is_open());
        assert!(!bridge.handle_key(KeyEvent::Enter, &mut app));
    }

    #[test]
    fn esc_dismisses_without_touching_input() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/m");
        bridge.sync(&app);
        assert!(bridge.handle_key(KeyEvent::Esc, &mut app));
        assert!(!bridge.is_open());
        assert_eq!(app.input(), "/m");
    }

    #[test]
    fn editing_keys_fall_through_for_app_then_sync() {
        let bridge = popup();
        let mut app = App::new();
        type_text(&mut app, "/");
        bridge.sync(&app);
        assert!(!bridge.handle_key(KeyEvent::Char('x'), &mut app));
        assert!(!bridge.handle_key(KeyEvent::Backspace, &mut app));
        assert!(!bridge.handle_key(KeyEvent::Left, &mut app));
        assert!(!bridge.handle_key(KeyEvent::Interrupt, &mut app));
        assert_eq!(app.input(), "/");
    }

    #[test]
    fn handle_key_when_closed_is_fallthrough() {
        let bridge = popup();
        let mut app = App::new();
        assert!(!bridge.handle_key(KeyEvent::Up, &mut app));
        assert!(!bridge.handle_key(KeyEvent::Enter, &mut app));
    }

    #[test]
    fn plugin_provides_bridge_through_di() {
        let ctx = Context::root();
        ctx.load(TuiCommandsPlugin).unwrap();
        let bridge: Arc<CommandPopup> = ctx.inject_key(KEY_COMMAND_POPUP).unwrap();
        assert!(!bridge.is_open());
    }

    #[test]
    fn plugin_needs_no_popup_service() {
        // Owns its surface: activates standalone, never parks on KEY_POPUP.
        let ctx = Context::root();
        let outcome = ctx.load(TuiCommandsPlugin).unwrap();
        assert!(matches!(outcome, harness_core::LoadOutcome::Activated));
        assert!(ctx.provider_of(&KEY_COMMAND_POPUP.into()).is_some());
    }

    #[test]
    fn surfaces_stay_independent_from_model_popup() {
        // Regression: sharing one Popup made the model bridge read "open"
        // while the slash list showed, swallowing typing and persisting
        // "/model" as a model id. Independent surfaces keep `is_open`
        // disjoint by construction.
        let commands = popup();
        let models = Popup::new();
        let mut app = App::new();
        type_text(&mut app, "/m");
        commands.sync(&app);
        assert!(commands.is_open());
        assert!(!models.is_open());
        assert_eq!(
            commands.snapshot().expect("open").items,
            vec!["/model".to_owned()]
        );
        // Typing keys still fall through to the app (never swallowed).
        assert!(!commands.handle_key(KeyEvent::Char('o'), &mut app));
        // First Enter completes into the input bar (consumed, no submit).
        assert!(commands.handle_key(KeyEvent::Enter, &mut app));
        assert_eq!(app.input(), "/model");
        // Second Enter on the exact command falls through to execute.
        assert!(!commands.handle_key(KeyEvent::Enter, &mut app));
        // The model surface never saw slash rows.
        assert!(!models.is_open());
        assert_eq!(models.snapshot(), None);
    }
}
