//! Terminal input service: reads crossterm key/mouse events and
//! translates them into `AppMsg` values on the app channel. Clicks,
//! drags, and anything unmapped are ignored; only scrolling matters.
//!
//! Provided as `Arc<Input>` under [`KEY_INPUT`](harness_contracts::KEY_INPUT)
//! so the TUI shell — or a headless driver in tests — can swap the event
//! source without touching the runtime.

use std::sync::Arc;

use crossterm::event::{
    Event as TermEvent, EventStream, KeyCode, KeyEvent as TermKeyEvent, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use futures::StreamExt;
use harness_contracts::KEY_INPUT;
use harness_core::{Context, Result};
use tokio::sync::mpsc;

use harness_tui_state::app::{AppMsg, KeyEvent};

/// Terminal event source. Spawns the reader task for a given app channel.
pub struct Input;

impl Input {
    /// Spawns the reader task. It exits when the channel closes (the runtime
    /// drops its receiver side on quit), taking the event stream with it.
    pub fn spawn(&self, tx: mpsc::Sender<AppMsg>) {
        tokio::spawn(async move {
            let mut events = EventStream::new();
            while let Some(event) = events.next().await {
                let Ok(event) = event else {
                    continue;
                };
                let Some(msg) = translate(event) else {
                    continue;
                };
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// Plugin providing the shared terminal input as `Arc<Input>` under
/// [`KEY_INPUT`](harness_contracts::KEY_INPUT).
pub struct InputPlugin;

impl harness_core::Plugin for InputPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("input").provides(KEY_INPUT)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        ctx.provide_key(KEY_INPUT, Arc::new(Input));
        Ok(())
    }
}

/// Maps one terminal event to an app message. Pure and unit-tested.
fn translate(event: TermEvent) -> Option<AppMsg> {
    match event {
        TermEvent::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return None;
            }
            map_key(key).map(AppMsg::Key)
        }
        TermEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => Some(AppMsg::ScrollUp),
            MouseEventKind::ScrollDown => Some(AppMsg::ScrollDown),
            _ => None,
        },
        _ => None,
    }
}

/// Maps a key press to the app's key model. Shift+Enter (Kitty keyboard
/// protocol) and Alt+Enter insert a newline; plain Enter submits.
fn map_key(key: TermKeyEvent) -> Option<KeyEvent> {
    // Ctrl+C interrupts the app; other ctrl-combos are ignored.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(KeyEvent::Interrupt),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => Some(KeyEvent::Newline),
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => Some(KeyEvent::Newline),
        KeyCode::Enter => Some(KeyEvent::Enter),
        KeyCode::Backspace => Some(KeyEvent::Backspace),
        KeyCode::Delete => Some(KeyEvent::Delete),
        KeyCode::Left => Some(KeyEvent::Left),
        KeyCode::Right => Some(KeyEvent::Right),
        KeyCode::Up => Some(KeyEvent::Up),
        KeyCode::Down => Some(KeyEvent::Down),
        KeyCode::Home => Some(KeyEvent::Home),
        KeyCode::End => Some(KeyEvent::End),
        KeyCode::PageUp => Some(KeyEvent::PageUp),
        KeyCode::PageDown => Some(KeyEvent::PageDown),
        KeyCode::Esc => Some(KeyEvent::Esc),
        KeyCode::Char(ch) => Some(KeyEvent::Char(ch)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_provides_input_through_di() {
        let ctx = Context::root();
        ctx.load(InputPlugin).unwrap();
        let input: Arc<Input> = ctx.inject_key(KEY_INPUT).unwrap();
        // Spawning would read the real terminal; just prove the service
        // resolves. Key translation below covers behavior.
        let _ = input;
    }

    fn press(code: KeyCode) -> TermKeyEvent {
        TermKeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn arrows_and_enter_map() {
        assert_eq!(map_key(press(KeyCode::Up)), Some(KeyEvent::Up));
        assert_eq!(map_key(press(KeyCode::Down)), Some(KeyEvent::Down));
        assert_eq!(map_key(press(KeyCode::Enter)), Some(KeyEvent::Enter));
    }

    #[test]
    fn shift_and_alt_enter_insert_newlines() {
        assert_eq!(
            map_key(TermKeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(KeyEvent::Newline)
        );
        assert_eq!(
            map_key(TermKeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            Some(KeyEvent::Newline)
        );
    }

    #[test]
    fn ctrl_c_interrupts_and_other_ctrl_is_ignored() {
        assert_eq!(
            map_key(TermKeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            map_key(TermKeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(KeyEvent::Interrupt)
        );
    }

    #[test]
    fn wheel_scrolls_and_clicks_are_ignored() {
        use crossterm::event::{MouseButton, MouseEvent};

        let wheel = |kind| {
            translate(TermEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }))
        };
        assert_eq!(wheel(MouseEventKind::ScrollUp), Some(AppMsg::ScrollUp));
        assert_eq!(wheel(MouseEventKind::ScrollDown), Some(AppMsg::ScrollDown));
        assert_eq!(
            wheel(MouseEventKind::Down(MouseButton::Left)),
            None,
            "clicks ignored"
        );
    }
}
