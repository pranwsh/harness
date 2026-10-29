//! Environment setup for shell subprocesses: no filtering.
//!
//! The shell plugin applies no guardrails. The subprocess inherits the agent
//! process environment by default; if the call provides explicit `env` vars,
//! the child gets a clean environment with only those vars. Filtering belongs
//! in a guardrail plugin via `tool.approval`.

use std::collections::HashMap;
use std::process::Command;

/// Apply env to a command about to spawn: inherit all, or clean + explicit.
pub fn apply_env(cmd: &mut Command, explicit: Option<&HashMap<String, String>>) {
    if let Some(vars) = explicit {
        cmd.env_clear();
        for (k, v) in vars {
            cmd.env(k, v);
        }
    }
}

/// Resolve a command through the current PATH before a one-shot clean environment is applied.
pub fn executable_path(name: &str) -> String {
    if name.contains('/') {
        return name.to_owned();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_owned())
}

/// Detach the child into its own process group/session (Unix).
pub fn detach_tty(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Kill a subprocess and its descendants that remain in its process group.
/// Falls back to killing just the child on non-Unix platforms.
pub fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        // Negative pid addresses the process group created by setsid above.
        // ESRCH means it already exited; child.wait() still reaps the leader.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_env_is_clean_and_inherits_only_requested_values() {
        let mut cmd = Command::new("echo");
        let vars = HashMap::from([("ONLY_THIS".to_owned(), "1".to_owned())]);
        apply_env(&mut cmd, Some(&vars));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.iter().any(
            |(k, v)| k.to_str() == Some("ONLY_THIS") && v.and_then(|x| x.to_str()) == Some("1")
        ));
        assert!(envs.iter().all(|(k, _)| k.to_str() != Some("PATH")));
    }

    #[test]
    fn default_environment_is_inherited() {
        let mut cmd = Command::new("echo");
        apply_env(&mut cmd, None);
        assert_eq!(cmd.get_envs().count(), 0);
    }
}
