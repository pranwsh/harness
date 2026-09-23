//! Environment setup for shell subprocesses: no filtering.
//!
//! The shell plugin applies no guardrails. The subprocess inherits the agent
//! process environment by default; if the call provides explicit `env` vars,
//! the child gets a clean environment with only those vars. Any filtering
//! (secret redaction, allowlists) belongs in a future guardrail plugin via
//! the `tool.approval` waterfall, not here.

use std::collections::HashMap;
use std::process::Command;

/// Apply env to a command about to spawn: inherit all, or clean + explicit.
pub fn apply_env(cmd: &mut Command, explicit: Option<&HashMap<String, String>>) {
    if let Some(vars) = explicit {
        // Explicit env = fully clean subprocess.
        cmd.env_clear();
        for (k, v) in vars {
            cmd.env(k, v);
        }
    }
    // Otherwise inherit everything; no denylist here.
}


/// Detach the child from our controlling terminal (no `/dev/tty`).
///
/// `stdin=null` + piped stdout/stderr is NOT enough: `sudo` (and
/// `su`/`passwd`/`ssh`/pagers) re-opens `/dev/tty` directly, painting
/// `Password:` over the TUI input box and stealing keystrokes from the
/// input task. A `setsid` in the child (new session, no ctty) makes that
/// open fail, so `sudo` exits fast with `no tty present` into captured
/// stderr instead of corrupting the screen. Unix-only; no-op elsewhere.
pub fn detach_tty(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // SAFETY: `setsid` is async-signal-safe; closure does nothing else.
        unsafe {
            cmd.pre_exec(|| {
                // Ignore errors: if already a session leader, parent keeps ctty,
                // but stdio is still piped/null so no screen corruption.
                let _ = libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_env_means_clean() {
        let mut cmd = Command::new("echo");
        let mut vars = HashMap::new();
        vars.insert("ONLY_THIS".to_owned(), "1".to_owned());
        apply_env(&mut cmd, Some(&vars));
        // env_clear + explicit: PATH must be gone from the child spec.
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| k.to_str() == Some("ONLY_THIS")
                    && v.map(|v| v == "1").unwrap_or(false)),
            "explicit var missing: {envs:?}"
        );
        assert!(
            envs.iter().all(|(k, _)| k.to_str() != Some("PATH")),
            "inherited var leaked: {envs:?}"
        );
    }

    #[test]
    fn no_explicit_env_inherits() {
        let mut cmd = Command::new("echo");
        apply_env(&mut cmd, None);
        // No env_clear, no removals: child spec carries no overrides.
        assert_eq!(cmd.get_envs().count(), 0);
    }
}
