//! Best-effort approval gate for obvious sudo shell commands.
//!
//! This is not a sandbox: shell indirection, scripts, other executables and
//! other tools can bypass command-text inspection. Ask mode denies if no
//! frontend responds to the request.
use harness_contracts::{
    CH_TOOL_APPROVAL, KEY_SUDO_APPROVAL, KEY_SUDO_POLICY, SudoApprovalApi, SudoApprovalHandle,
    SudoApprovalRequest, SudoMode, SudoPolicyApi, SudoPolicyHandle, ToolApproval,
};
use harness_core::{Context, Result};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Default)]
pub struct SudoPolicy {
    modes: Mutex<HashMap<String, SudoMode>>,
}
impl SudoPolicyApi for SudoPolicy {
    fn mode(&self, session_id: &str) -> SudoMode {
        self.modes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }
    fn set_mode(&self, session_id: &str, mode: SudoMode) {
        if session_id.is_empty() {
            return;
        }
        let mut modes = self.modes.lock().unwrap_or_else(|e| e.into_inner());
        if mode == SudoMode::Block {
            modes.remove(session_id);
        } else {
            modes.insert(session_id.to_owned(), mode);
        }
    }
    fn reset(&self, session_id: &str) {
        self.modes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }
}

#[derive(Default)]
struct ApprovalQueue {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, (SudoApprovalRequest, tokio::sync::oneshot::Sender<bool>)>>,
}
impl SudoApprovalApi for ApprovalQueue {
    fn pending(&self, session_id: &str) -> Option<SudoApprovalRequest> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|(r, tx)| r.session_id == session_id && !tx.is_closed())
            .map(|(r, _)| r.clone())
            .min_by_key(|r| r.id)
    }
    fn respond(&self, session_id: &str, id: u64, allow: bool) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if p.get(&id).is_some_and(|(r, _)| r.session_id == session_id) {
            if let Some((_, tx)) = p.remove(&id) {
                let _ = tx.send(allow);
            }
        }
    }
    fn cancel_session(&self, session_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (r, _)| r.session_id != session_id);
    }
}
impl ApprovalQueue {
    async fn ask(&self, session_id: &str, details: String) -> bool {
        // Refuse to display incomplete details or terminal control sequences.
        if details.len() > 65536 || details.chars().any(char::is_control) {
            return false;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.retain(|_, (_, tx)| !tx.is_closed());
            if p.len() >= 16 {
                return false;
            }
            p.insert(
                id,
                (
                    SudoApprovalRequest {
                        id,
                        session_id: session_id.to_owned(),
                        details,
                    },
                    tx,
                ),
            );
        }
        let result = matches!(
            tokio::time::timeout(std::time::Duration::from_secs(60), rx).await,
            Ok(Ok(true))
        );
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        result
    }
}

/// Conservative UX filter, deliberately not presented as enforcement.
fn suspect(call: &harness_contracts::ToolCall) -> bool {
    if call.name == "shell_sudo_exec" {
        return true;
    }
    if !matches!(call.name.as_str(), "shell_exec" | "shell_start") {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
        return true;
    };
    let Some(command) = value.get("command").and_then(|v| v.as_str()) else {
        return true;
    };
    command.to_ascii_lowercase().contains("sudo")
}

pub struct SudoPolicyPlugin;
impl harness_core::Plugin for SudoPolicyPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("sudo-policy")
            .provides(KEY_SUDO_POLICY)
            .provides(KEY_SUDO_APPROVAL)
            .waterfalls::<ToolApproval>(CH_TOOL_APPROVAL)
    }
    fn build(&self, ctx: Context) -> Result<()> {
        let policy = Arc::new(SudoPolicy::default());
        let queue = Arc::new(ApprovalQueue::default());
        let gate_queue = queue.clone();
        let gate = policy.clone();
        ctx.on_waterfall_key::<ToolApproval, _, _>(CH_TOOL_APPROVAL, move |a| {
            let policy = gate.clone();
            let queue = gate_queue.clone();
            async move {
                let mut a = (*a).clone();
                if a.denied.is_none() && suspect(&a.call) {
                    match policy.mode(&a.session_id) {
                        SudoMode::Block => a.denied = Some("sudo requests blocked by session policy (shell detection is not a sandbox)".into()),
                        SudoMode::Ask => {
                            // Arbitrary Bash cannot be displayed as a trustworthy argv.
                            if a.call.name != "shell_sudo_exec" {
                                a.denied = Some("use shell_sudo_exec for interactive approval; shell detection is not a sandbox".into());
                            } else if !queue.ask(&a.session_id, a.call.arguments.clone()).await || policy.mode(&a.session_id) != SudoMode::Ask {
                                a.denied = Some("sudo confirmation denied or unavailable".into());
                            }
                        },
                        SudoMode::AllowForSession => {},
                    }
                }
                a
            }
        })?;
        ctx.provide_key(KEY_SUDO_POLICY, Arc::new(SudoPolicyHandle(policy)));
        ctx.provide_key(KEY_SUDO_APPROVAL, Arc::new(SudoApprovalHandle(queue)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_contracts::{KEY_TOOLS, ToolCall};
    use harness_tools::{Tools, ToolsPlugin};
    #[test]
    fn modes_are_session_scoped_and_reset() {
        let p = SudoPolicy::default();
        p.set_mode("one", SudoMode::AllowForSession);
        assert_eq!(p.mode("one"), SudoMode::AllowForSession);
        assert_eq!(p.mode("two"), SudoMode::Block);
        p.reset("one");
        assert_eq!(p.mode("one"), SudoMode::Block);
    }
    #[test]
    fn explicit_sudo_is_always_subject_to_policy() {
        let call = ToolCall {
            id: "2".into(),
            name: "shell_sudo_exec".into(),
            arguments: r#"{"program":"id"}"#.into(),
        };
        assert!(suspect(&call));
    }

    #[tokio::test]
    async fn confirmation_is_session_and_request_bound() {
        let queue = Arc::new(ApprovalQueue::default());
        let task = tokio::spawn({
            let q = queue.clone();
            async move { q.ask("one", "id".into()).await }
        });
        tokio::task::yield_now().await;
        let request = queue.pending("one").unwrap();
        queue.respond("two", request.id, true);
        assert!(queue.pending("one").is_some());
        queue.respond("one", request.id + 1, true);
        assert!(queue.pending("one").is_some());
        queue.respond("one", request.id, true);
        assert!(task.await.unwrap());
        assert!(queue.pending("one").is_none());
    }

    #[tokio::test]
    async fn cancellation_denies_pending_request() {
        let queue = Arc::new(ApprovalQueue::default());
        let task = tokio::spawn({
            let q = queue.clone();
            async move { q.ask("one", "id".into()).await }
        });
        tokio::task::yield_now().await;
        queue.cancel_session("one");
        assert!(!task.await.unwrap());
    }

    #[tokio::test]
    async fn default_and_ask_fail_closed() {
        let ctx = Context::root();
        ctx.load(ToolsPlugin).unwrap();
        ctx.load(SudoPolicyPlugin).unwrap();
        let tools: Arc<Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
        let p: Arc<SudoPolicyHandle> = ctx.inject_key(KEY_SUDO_POLICY).unwrap();
        let call = ToolCall {
            id: "1".into(),
            name: "shell_exec".into(),
            arguments: r#"{"command":"sudo id"}"#.into(),
        };
        assert!(
            tools
                .execute("a", "s", 1, call.clone())
                .await
                .unwrap_err()
                .message
                .contains("blocked")
        );
        p.0.set_mode("s", SudoMode::Ask);
        assert!(
            tools
                .execute("a", "s", 1, call)
                .await
                .unwrap_err()
                .message
                .contains("shell_sudo_exec")
        );
    }
}
