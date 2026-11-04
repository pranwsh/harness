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
    Request {
        id: u64,
        details: String,
        page: usize,
        width: usize,
        reviewed: usize,
    },
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
    /// Lay out approval details without truncation. A resize restarts review.
    pub fn sync(&self, session: &str, width: usize, height: usize) {
        let request = self.approvals.0.pending(session);
        let mut mode = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(State::Request { id, .. }) = mode.as_ref() {
            if !request.as_ref().is_some_and(|r| r.id == *id) {
                self.popup.close();
                *mode = None;
            }
        }
        if width < 25 || height < 5 {
            if let Some(r) = request {
                self.approvals.0.respond(session, r.id, false);
            }
            if matches!(*mode, Some(State::Request { .. })) {
                self.popup.close();
                *mode = None;
            }
            return;
        }
        let content_width = width.saturating_sub(4);
        if let Some(State::Request {
            details,
            page,
            width: old_width,
            reviewed,
            ..
        }) = mode.as_mut()
        {
            if *old_width != content_width {
                *old_width = content_width;
                *page = 0;
                *reviewed = 0;
                self.show_info(details, *page, content_width);
            }
            return;
        }
        if mode.is_some() {
            return;
        }
        if let Some(r) = request {
            // Escape non-ASCII and controls, keeping the complete JSON unambiguous.
            let details: String = r
                .details
                .chars()
                .flat_map(|c| {
                    if c.is_ascii_graphic() || c == ' ' {
                        c.to_string().chars().collect::<Vec<_>>()
                    } else {
                        c.escape_default().collect()
                    }
                })
                .collect();
            self.popup.open(
                " approval: Left/Right to review ",
                vec!["No".into(), "Yes".into()],
                0,
                None,
                vec![],
            );
            self.show_info(&details, 0, content_width);
            *mode = Some(State::Request {
                id: r.id,
                details,
                page: 0,
                width: content_width,
                reviewed: 0,
            });
        }
    }

    fn show_info(&self, details: &str, page: usize, width: usize) {
        let start = page * width;
        let end = (start + width).min(details.len());
        let total = details.len().max(1).div_ceil(width);
        self.popup.set_info(Some(details[start..end].to_owned()));
        self.popup
            .set_title(format!(" approval {}/{} • ←/→ ", page + 1, total));
    }

    pub fn handle(&self, session: &str, key: KeyEvent, app: &mut App) -> bool {
        let mut state = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = state.clone() else {
            return false;
        };
        match key {
            KeyEvent::Up => self.popup.move_up(),
            KeyEvent::Down => self.popup.move_down(),
            KeyEvent::Right | KeyEvent::PageDown => {
                if let Some(State::Request {
                    details,
                    page,
                    width,
                    reviewed,
                    ..
                }) = state.as_mut()
                {
                    let last = details.len().saturating_sub(1) / *width;
                    if *page < last {
                        *page += 1;
                        *reviewed = (*reviewed).max(*page);
                        self.show_info(details, *page, *width);
                    }
                }
            }
            KeyEvent::Left | KeyEvent::PageUp => {
                if let Some(State::Request {
                    details,
                    page,
                    width,
                    ..
                }) = state.as_mut()
                {
                    if *page > 0 {
                        *page -= 1;
                        self.show_info(details, *page, *width);
                    }
                }
            }
            KeyEvent::Interrupt => {
                if let State::Request { id, .. } = s {
                    self.approvals.0.respond(session, id, false);
                }
                self.popup.close();
                *state = None;
                return false;
            }
            KeyEvent::Esc => {
                if let State::Request { id, .. } = s {
                    self.approvals.0.respond(session, id, false);
                }
                self.popup.close();
                *state = None;
            }
            KeyEvent::Enter => {
                let chosen = self.popup.selected_item().unwrap_or_default();
                match s {
                    State::Request {
                        id,
                        details,
                        width,
                        reviewed,
                        ..
                    } => {
                        let last = details.len().saturating_sub(1) / width;
                        if chosen == "Yes" && reviewed < last {
                            app.update(AppMsg::Notice(
                                "Review all request pages with Right or PageDown before approving"
                                    .into(),
                            ));
                            return true;
                        }
                        self.approvals.0.respond(session, id, chosen == "Yes");
                        self.popup.close();
                        *state = None;
                    }
                    State::Mode => {
                        let mode = match chosen.as_str() {
                            "Ask" => SudoMode::Ask,
                            "AllowForSession" => SudoMode::AllowForSession,
                            _ => SudoMode::Block,
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

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_TOOLS, ToolCall};
    use harness_sudo_policy::SudoPolicyPlugin;
    use harness_tools::{Tools, ToolsPlugin};

    #[tokio::test]
    async fn long_request_must_be_reviewed_before_yes() {
        let ctx = Context::root();
        ctx.load(ToolsPlugin).unwrap();
        ctx.load(SudoPolicyPlugin).unwrap();
        let policy: Arc<SudoPolicyHandle> = ctx.inject_key(KEY_SUDO_POLICY).unwrap();
        let approvals: Arc<SudoApprovalHandle> = ctx.inject_key(KEY_SUDO_APPROVAL).unwrap();
        policy.0.set_mode("s", SudoMode::Ask);
        let popup = SudoPopup::new(policy, approvals);
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let call = ToolCall {
            id: "1".into(),
            name: "shell_sudo_exec".into(),
            arguments: format!(r#"{{"program":"{}"}}"#, "x".repeat(150)),
        };
        let task = tokio::spawn(async move { tools.execute("a", "s", 1, call).await });
        tokio::task::yield_now().await;
        let mut app = App::new();
        popup.sync("s", 50, 10);
        assert!(popup.snapshot().unwrap().info.is_some());
        popup.handle("s", KeyEvent::Down, &mut app);
        popup.handle("s", KeyEvent::Enter, &mut app);
        assert!(popup.active());
        for _ in 0..10 {
            popup.handle("s", KeyEvent::Right, &mut app);
        }
        popup.handle("s", KeyEvent::Enter, &mut app);
        assert!(!popup.active());
        assert_eq!(task.await.unwrap().unwrap_err().message, "unknown tool");
    }
}
