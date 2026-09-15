//! Slash-command vocabulary: the static list the autocomplete popup
//! filters. Single source of truth so `tui-state` (execution), the
//! `tui-commands` provider (filtering), and tests can never drift apart.
//! No behavior lives here — just names and pure prefix matching.
//! `/model` and `/sessions` are hijacked by the TUI shell into their
//! provider popups; any other slash line is an inline `unknown command`
//! error at submit time (see `App::submit`).

/// The slash commands. The popup shows them on a bare `/` and
/// prefix-filters them as the user types.
pub const ALL_COMMANDS: [&str; 2] = ["/model", "/sessions"];

/// Popup title, with the surrounding spaces the border title expects.
pub const COMMANDS_TITLE: &str = " commands ";

/// Prefix-filter `ALL_COMMANDS` by `candidate` (which must already be the
/// slash token, e.g. `"/c"`). Returns matches in `ALL_COMMANDS` order.
pub fn filter_commands(candidate: &str) -> Vec<String> {
    ALL_COMMANDS
        .iter()
        .filter(|cmd| cmd.starts_with(candidate))
        .map(|cmd| cmd.to_string())
        .collect()
}

/// Extracts the slash candidate from an input line: trimmed-start text
/// that is a single `/`-led token with no whitespace. Returns `None` for
/// non-slash input, multi-word lines, or multi-line input — all of which
/// must close the popup.
pub fn slash_candidate(input: &str) -> Option<&str> {
    let trimmed = input.trim_start();
    let token = trimmed.split_whitespace().next()?;
    if !token.starts_with('/') {
        return None;
    }
    // The whole line must be exactly this token (modulo surrounding
    // whitespace): trailing args or extra lines disqualify.
    if trimmed.trim() != token {
        return None;
    }
    if token.contains('\n') {
        return None;
    }
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_slash_matches_everything() {
        assert_eq!(
            filter_commands("/"),
            vec!["/model".to_owned(), "/sessions".to_owned()]
        );
    }

    #[test]
    fn prefix_filters() {
        assert_eq!(filter_commands("/m"), vec!["/model".to_owned()]);
        assert_eq!(filter_commands("/model"), vec!["/model".to_owned()]);
        assert_eq!(filter_commands("/s"), vec!["/sessions".to_owned()]);
        assert_eq!(
            filter_commands("/sessions"),
            vec!["/sessions".to_owned()]
        );
        assert_eq!(filter_commands("/c"), Vec::<String>::new());
        assert_eq!(filter_commands("/bogus"), Vec::<String>::new());
    }

    #[test]
    fn candidate_requires_single_slash_token() {
        assert_eq!(slash_candidate("/"), Some("/"));
        assert_eq!(slash_candidate("  /c"), Some("/c"));
        assert_eq!(slash_candidate("/model"), Some("/model"));
        assert_eq!(slash_candidate("hello"), None);
        assert_eq!(slash_candidate("hi /c"), None);
        assert_eq!(slash_candidate("/model "), Some("/model"));
        assert_eq!(slash_candidate("/model x"), None);
        assert_eq!(slash_candidate("/a\n/b"), None);
        assert_eq!(slash_candidate(""), None);
    }
}
