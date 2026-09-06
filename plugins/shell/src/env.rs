//! Environment filtering: glob-based denylist + clean-mode support.
//!
//! Matching is ASCII case-insensitive `*`-glob (no regex -> no ReDoS,
//! O(p+t) two-pointer). `std::env::vars()` is collected once per spawn.

use std::collections::HashMap;
use std::process::Command;

use harness_config::ShellEnvMode;

/// True if `key` matches glob `pattern` (`*` = any run). Case-insensitive.
pub fn matches_glob(pattern: &str, key: &str) -> bool {
    // Cheap path: exact (still case-insensitive).
    if !pattern.as_bytes().contains(&b'*') {
        return key.len() == pattern.len()
            && key
                .bytes()
                .zip(pattern.bytes())
                .all(|(a, b)| a.eq_ignore_ascii_case(&b));
    }
    wildcard_match(pattern.as_bytes(), key.as_bytes())
}

/// Classic two-pointer wildcard match with `*` (and `?` for one char).
fn wildcard_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut px, mut tx) = (0usize, 0usize);
    let (mut star, mut match_idx) = (None::<usize>, 0usize);
    while tx < text.len() {
        if px < pat.len() && (pat[px] == b'?' || pat[px].eq_ignore_ascii_case(&text[tx])) {
            px += 1;
            tx += 1;
        } else if px < pat.len() && pat[px] == b'*' {
            star = Some(px);
            match_idx = tx;
            px += 1;
        } else if let Some(s) = star {
            px = s + 1;
            match_idx += 1;
            tx = match_idx;
        } else {
            return false;
        }
    }
    while px < pat.len() && pat[px] == b'*' {
        px += 1;
    }
    px == pat.len()
}

/// True if `key` matches any denied pattern.
pub fn is_denied(key: &str, denied: &[String]) -> bool {
    denied.iter().any(|p| matches_glob(p, key))
}

/// Apply env policy to a command about to spawn.
pub fn apply_env(
    cmd: &mut Command,
    explicit: Option<&HashMap<String, String>>,
    mode: ShellEnvMode,
    denied: &[String],
) {
    if let Some(vars) = explicit {
        // Explicit env = fully clean subprocess.
        cmd.env_clear();
        for (k, v) in vars {
            cmd.env(k, v);
        }
        return;
    }
    match mode {
        ShellEnvMode::Clean => {
            cmd.env_clear();
        }
        ShellEnvMode::InheritFiltered => {
            if denied.is_empty() {
                return;
            }
            // Command inherits by default; remove denied matches.
            // Single pass over a snapshot to avoid borrow issues.
            for (k, _) in std::env::vars() {
                if is_denied(&k, denied) {
                    cmd.env_remove(&k);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_star_patterns() {
        assert!(matches_glob("*API_KEY*", "OPENAI_API_KEY"));
        assert!(matches_glob("OPENAI_*", "OPENAI_API_KEY"));
        assert!(!matches_glob("OPENAI_*", "ANTHROPIC_KEY"));
        assert!(matches_glob("HF_TOKEN", "hf_token")); // case-insensitive
        assert!(!matches_glob("HF_TOKEN", "HF_TOKEN_X"));
        assert!(matches_glob("*TOKEN*", "gh_token"));
        assert!(!matches_glob("*TOKEN*", "PATH"));
    }

    #[test]
    fn denied_detection() {
        let denied = vec!["*API_KEY*".to_owned(), "HF_TOKEN".to_owned()];
        assert!(is_denied("OPENAI_API_KEY", &denied));
        assert!(is_denied("hf_token", &denied));
        assert!(!is_denied("PATH", &denied));
    }

    #[test]
    fn explicit_env_means_clean() {
        let mut cmd = Command::new("echo");
        let mut vars = HashMap::new();
        vars.insert("ONLY_THIS".to_owned(), "1".to_owned());
        apply_env(
            &mut cmd,
            Some(&vars),
            ShellEnvMode::InheritFiltered,
            &["*".to_owned()],
        );
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
}
