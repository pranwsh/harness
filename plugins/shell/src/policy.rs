//! Best-effort command policy: allowlist, denylist, interactive block,
//! dangerous-operator scan, and shell `-c` recursion.
//!
//! All checks are pure, allocation-free on the hot path (HashSet lookups +
//! one linear scan), and run before any spawn. This is a safety net, NOT a
//! sandbox: `cp sh /tmp/x` style bypasses exist. Run untrusted work in a
//! container.

use std::collections::HashSet;

use harness_config::ShellConfig;

/// Compiled policy built once at plugin build; cheap to clone via `Arc`.
#[derive(Debug)]
pub struct CompiledPolicy {
    allow: HashSet<Box<str>>,
    deny: HashSet<Box<str>>,
    interactive: HashSet<Box<str>>,
    shells: HashSet<Box<str>>,
    /// Longest-first so `>>`/`||`/`&&`/`$(` match before prefixes.
    blocked_ops: Vec<String>,
    allow_shell: bool,
}

impl CompiledPolicy {
    pub fn compile(cfg: &ShellConfig) -> Self {
        let mut blocked_ops = cfg.blocked_operators.clone();
        // Longest-first avoids `>` shadowing `>>` in diagnostics.
        blocked_ops.sort_by_key(|a| std::cmp::Reverse(a.len()));
        CompiledPolicy {
            allow: cfg.allowlist.iter().map(|s| s.as_str().into()).collect(),
            deny: cfg.denylist.iter().map(|s| s.as_str().into()).collect(),
            interactive: cfg
                .interactive_deny
                .iter()
                .map(|s| s.as_str().into())
                .collect(),
            shells: cfg
                .shell_binaries
                .iter()
                .map(|s| s.as_str().into())
                .collect(),
            blocked_ops,
            allow_shell: cfg.allow_shell,
        }
    }

    /// Validate `argv` (`argv[0]` = executable). Returns the exe basename.
    pub fn check(&self, argv: &[String]) -> Result<String, String> {
        self.check_depth(argv, 0)
    }

    fn check_depth(&self, argv: &[String], depth: u8) -> Result<String, String> {
        let first = argv
            .first()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "empty command: provide argv like [\"git\", \"status\"]".to_owned())?;
        let exe = basename(first);
        if exe.is_empty() {
            return Err("empty executable".to_owned());
        }
        // Deny wins over everything (covers overlap with allow/interactive).
        if self.deny.contains(exe.as_str()) {
            return Err(format!("denied executable `{exe}` (denylist)"));
        }
        if self.interactive.contains(exe.as_str()) {
            return Err(format!(
                "blocked interactive/TTY command `{exe}` (would hang headless agent)"
            ));
        }
        let is_shell = self.shells.contains(exe.as_str());
        if is_shell {
            if !self.allow_shell {
                return Err(format!(
                    "shell `{exe}` is blocked (allow_shell=false); use argv like [\"git\", \"status\"]"
                ));
            }
            // Shell explicitly allowed: still require allowlisting + inspect -c.
            if !self.allow.contains(exe.as_str()) {
                return Err(format!("executable `{exe}` is not in the allowlist"));
            }
            if depth < 3
                && let Some(script) = extract_dash_c(argv)
            {
                let inner = lex_first(&script);
                if !inner.is_empty() {
                    let inner_argv: Vec<String> = lex_all(&script).collect();
                    if !inner_argv.is_empty() {
                        self.check_depth(&inner_argv, depth + 1)
                            .map_err(|e| format!("shell -c payload rejected: {e}"))?;
                    } else {
                        // Unlexable but non-empty script: fall through to
                        // operator scan below which will likely reject it.
                    }
                    let _ = inner;
                }
            }
        } else if !self.allow.contains(exe.as_str()) {
            return Err(format!("executable `{exe}` is not in the allowlist"));
        }
        // Operator scan over the raw args (quote-aware, best-effort).
        for arg in argv.iter().skip(1) {
            if let Some(op) = find_unquoted_op(arg, &self.blocked_ops) {
                return Err(format!(
                    "dangerous operator `{op}` rejected (argv mode has no shell; split into separate calls)"
                ));
            }
        }
        // Also scan exe itself (e.g. `bash -c` smuggled as one string is
        // caught here when caller passes a single joined string).
        if argv.len() == 1
            && let Some(op) = find_unquoted_op(first, &self.blocked_ops)
        {
            return Err(format!(
                "dangerous operator `{op}` rejected (pass argv array, not a shell string)"
            ));
        }
        Ok(exe)
    }
}

/// Basename of a path: after last `/` or `\`. No allocation on miss.
pub fn basename(path: &str) -> String {
    let mut base = path;
    if let Some(i) = base.rfind('/') {
        base = &base[i + 1..];
    }
    if let Some(i) = base.rfind('\\') {
        base = &base[i + 1..];
    }
    base.to_owned()
}

/// Extract the script of `sh -c <script>` / `sh -xc <script>`.
fn extract_dash_c(argv: &[String]) -> Option<String> {
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "-c" {
            return argv.get(i + 1).cloned();
        }
        // Combined short flags like `-xc`, `-lc`.
        if a.starts_with('-') && !a.starts_with("--") && a.contains('c') && a.len() > 2 {
            return argv.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

/// Minimal shell-ish lexer: split on ASCII whitespace respecting
/// single/double quotes and backslash escapes.
fn lex_all(script: &str) -> impl Iterator<Item = String> + '_ {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_tok = false;
    let mut quote = 0u8; // 0, b'\'', b'"'
    let mut chars = script.bytes().peekable();
    while let Some(b) = chars.next() {
        if quote == b'\'' {
            if b == b'\'' {
                quote = 0;
            } else {
                cur.push(b as char);
            }
            in_tok = true;
            continue;
        }
        if quote == b'"' {
            if b == b'"' {
                quote = 0;
            } else if b == b'\\' {
                if let Some(n) = chars.next() {
                    cur.push(n as char);
                }
            } else {
                cur.push(b as char);
            }
            in_tok = true;
            continue;
        }
        match b {
            b'\'' | b'"' => {
                quote = b;
                in_tok = true;
            }
            b'\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n as char);
                }
                in_tok = true;
            }
            b' ' | b'\t' | b'\n' | b'\r' => {
                if in_tok {
                    tokens.push(std::mem::take(&mut cur));
                    in_tok = false;
                }
            }
            _ => {
                cur.push(b as char);
                in_tok = true;
            }
        }
    }
    if in_tok {
        tokens.push(cur);
    }
    tokens.into_iter()
}

fn lex_first(script: &str) -> String {
    lex_all(script).next().unwrap_or_default()
}

/// Find the first blocked operator occurring outside quotes.
/// Returns the operator string. Single linear pass, O(len).
fn find_unquoted_op<'a>(arg: &str, ops: &'a [String]) -> Option<&'a str> {
    // Fast path: skip scan if no ASCII punct that starts an op.
    let bytes = arg.as_bytes();
    let mut quote = 0u8;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if quote == b'\'' {
            if b == b'\'' {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if quote == b'"' {
            if b == b'"' {
                quote = 0;
            } else if b == b'\\' {
                i += 1; // skip escaped char inside dquotes
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => {
                quote = b;
                i += 1;
            }
            b'\\' => {
                i += 2; // skip escaped char outside quotes
            }
            _ => {
                // Check operators at this position (longest-first).
                let rest = &arg[i..];
                let mut hit: Option<&'a str> = None;
                for op in ops {
                    if rest.starts_with(op.as_str()) {
                        hit = Some(op.as_str());
                        break;
                    }
                }
                if hit.is_some() {
                    return hit;
                }
                // Advance by UTF-8 char width to stay on boundaries.
                let w = utf8_width(bytes[i]);
                i += w;
            }
        }
    }
    None
}

#[inline]
fn utf8_width(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_config::ShellConfig;

    fn policy() -> CompiledPolicy {
        CompiledPolicy::compile(&ShellConfig::default())
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn allowlist_permits_default_tools() {
        let p = policy();
        assert!(p.check(&argv(&["git", "status"])).is_ok());
        assert!(p.check(&argv(&["ls", "-la"])).is_ok());
        assert_eq!(p.check(&argv(&["/usr/bin/git", "log"])).unwrap(), "git");
    }

    #[test]
    fn empty_command_rejected() {
        let p = policy();
        assert!(p.check(&[]).is_err());
        assert!(p.check(&argv(&["  "])).is_err());
    }

    #[test]
    fn denylist_wins_and_unknown_denied() {
        let p = policy();
        assert!(p.check(&argv(&["rm", "-rf", "/"])).is_err());
        assert!(p.check(&argv(&["curl", "https://x"])).is_err());
    }

    #[test]
    fn interactive_blocked() {
        let p = policy();
        for cmd in ["sudo", "ssh", "vim", "less", "top"] {
            assert!(p.check(&argv(&[cmd])).is_err(), "{cmd}");
        }
    }

    #[test]
    fn operators_rejected_outside_quotes() {
        let p = policy();
        assert!(p.check(&argv(&["echo", "a>b"])).is_err());
        assert!(p.check(&argv(&["echo", "a|b"])).is_err());
        assert!(p.check(&argv(&["echo", "$(whoami)"])).is_err());
        // Quoted operators are fine.
        assert!(p.check(&argv(&["echo", "'a>b'"])).is_ok());
        assert!(p.check(&argv(&["echo", "\"a|b\""])).is_ok());
        // `;` is inert in argv-exec (no shell) so it is not in the default
        // set — e.g. `python3 -c "a; b"` must work — but users can add it.
        assert!(p.check(&argv(&["echo", "hi;rm"])).is_ok());
        let strict_cfg = ShellConfig {
            blocked_operators: [";", "&&"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            ..ShellConfig::default()
        };
        let strict = CompiledPolicy::compile(&strict_cfg);
        assert!(strict.check(&argv(&["echo", "hi;rm"])).is_err());
        assert!(strict.check(&argv(&["echo", "a&&b"])).is_err());
    }

    #[test]
    fn shells_blocked_by_default() {
        let p = policy();
        assert!(p.check(&argv(&["sh", "-c", "echo hi"])).is_err());
        assert!(p.check(&argv(&["bash", "-c", "echo hi"])).is_err());
    }

    #[test]
    fn shell_recursion_inspected_when_allowed() {
        let mut allowlist = ShellConfig::default().allowlist;
        allowlist.push("sh".to_owned());
        let cfg = ShellConfig {
            allow_shell: true,
            allowlist,
            ..ShellConfig::default()
        };
        let p = CompiledPolicy::compile(&cfg);
        assert!(p.check(&argv(&["sh", "-c", "echo hi"])).is_ok());
        assert!(p.check(&argv(&["sh", "-c", "rm -rf /"])).is_err());
    }

    #[test]
    fn policy_check_is_fast() {
        let p = policy();
        let a = argv(&["git", "status", "--short"]);
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            let _ = p.check(&a);
        }
        assert!(start.elapsed().as_millis() < 500, "too slow");
    }
}
