//! Trusted TUI bridge for session sudo mode and confirmations.
use harness_contracts::{
    KEY_SUDO_APPROVAL, KEY_SUDO_POLICY, KEY_SUDO_POPUP, SudoApprovalHandle, SudoMode,
    SudoPolicyHandle,
};
use harness_core::{Context, Result};
use harness_tui_popup::{ActivePopup, Popup};
use harness_tui_state::app::{App, AppMsg, KeyEvent};
use std::sync::{Arc, Mutex};

pub struct SudoPopup {
    policy: Arc<SudoPolicyHandle>,
    approvals: Arc<SudoApprovalHandle>,
    popup: Popup,
    mode: Mutex<Option<State>>,
}
#[derive(Clone)]
enum State {
    Mode,
    Enable,
    Request(u64),
}
impl SudoPopup {
    pub fn new(policy: Arc<SudoPolicyHandle>, approvals: Arc<SudoApprovalHandle>) -> Self {
        Self {
            policy,
            approvals,
            popup: Popup::new(),
            mode: Mutex::new(None),
        }
    }
    pub fn snapshot(&self) -> Option<ActivePopup> {
        self.popup.snapshot()
    }
    pub fn active(&self) -> bool {
        self.popup.is_open()
    }
    pub fn open(&self, session: &str, app: &mut App) {
        app.clear_input();
        let current = format!("{:?}", self.policy.0.mode(session));
        self.popup.open(
            " sudo policy (not a sandbox) ",
            vec!["Block".into(), "Ask".into(), "AllowForSession".into()],
            0,
            Some(current),
            vec![],
        );
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = Some(State::Mode);
    }
    /// Never allow when the exact request cannot fit in the popup title.
    pub fn sync(&self, session: &str, width: usize) {
        let request = self.approvals.0.pending(session);
        let mut mode = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(State::Request(id)) = mode.as_ref() {
            if !request.as_ref().is_some_and(|r| r.id == *id) {
                self.popup.close();
                *mode = None;
            }
        }
        if mode.is_some() {
            return;
        }
        if let Some(r) = request {
            // ASCII only to make byte length equal display width; reject rather than truncate.
            if !r.details.is_ascii() || r.details.len() + 20 > width || r.details.len() > 120 {
                self.approvals.0.respond(session, r.id, false);
                return;
            }
            self.popup.open(
                format!(" sudo: {} ", r.details),
                vec!["No".into(), "Yes".into()],
                0,
                None,
                vec![],
            );
            *mode = Some(State::Request(r.id));
        }
    }
    pub fn handle(&self, session: &str, key: KeyEvent, app: &mut App) -> bool {
        let mut state = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = state.clone() else {
            return false;
        };
        match key {
            KeyEvent::Up => self.popup.move_up(),
            KeyEvent::Down => self.popup.move_down(),
            KeyEvent::Interrupt => {
                if let State::Request(id) = s {
                    self.approvals.0.respond(session, id, false);
                }
                self.popup.close();
                *state = None;
                return false;
            }
            KeyEvent::Esc => {
                if let State::Request(id) = s {
                    self.approvals.0.respond(session, id, false);
                }
                self.popup.close();
                *state = None;
            }
            KeyEvent::Enter => {
                let chosen = self.popup.selected_item().unwrap_or_default();
                match s {
                    State::Request(id) => {
                        self.approvals.0.respond(session, id, chosen == "Yes");
                        self.popup.close();
                        *state = None;
                    }
                    State::Enable => {
                        if chosen == "Yes" {
                            self.policy.0.set_mode(session, SudoMode::AllowForSession);
                            app.update(AppMsg::Notice("sudo: allowed for this session (not a sandbox; OS authorization still applies)".into()));
                        }
                        self.popup.close();
                        *state = None;
                    }
                    State::Mode if chosen == "AllowForSession" => {
                        self.popup.open(
                            " allow for this session? OS authorization still applies ",
                            vec!["No".into(), "Yes".into()],
                            0,
                            None,
                            vec![],
                        );
                        *state = Some(State::Enable);
                    }
                    State::Mode => {
                        let mode = if chosen == "Ask" {
                            SudoMode::Ask
                        } else {
                            SudoMode::Block
                        };
                        self.policy.0.set_mode(session, mode);
                        app.update(AppMsg::Notice(format!(
                            "sudo: {mode:?} (shell detection is not a sandbox)"
                        )));
                        self.popup.close();
                        *state = None;
                    }
                }
            }
            _ => {}
        }
        true
    }
    pub fn reset(&self, session: &str) {
        self.approvals.0.cancel_session(session);
        self.policy.0.reset(session);
        self.popup.close();
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}
pub struct TuiSudoPlugin;
impl harness_core::Plugin for TuiSudoPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("tui-sudo")
            .injects(KEY_SUDO_POLICY)
            .injects(KEY_SUDO_APPROVAL)
            .provides(KEY_SUDO_POPUP)
    }
    fn build(&self, ctx: Context) -> Result<()> {
        let policy = ctx.inject_key(KEY_SUDO_POLICY)?;
        let approvals = ctx.inject_key(KEY_SUDO_APPROVAL)?;
        ctx.provide_key(KEY_SUDO_POPUP, Arc::new(SudoPopup::new(policy, approvals)));
        Ok(())
    }
}
