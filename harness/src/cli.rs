// Strict, minimal command-line parsing for the `harness` binary.
// Only one flag exists in v1: `--config PATH` (exact match, separate value).
// Anything else is a hard error so future flags fail loudly instead of
// being silently ignored.

/// Parsed command-line arguments.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliArgs {
    /// Override for the config file path. `None` means the default from
    /// [`harness_config::config_path`].
    pub config: Option<String>,
}

/// One-line usage printed alongside every CLI error.
pub fn usage() -> &'static str {
    "Usage: harness [--config PATH]"
}

/// Parses `std::env::args().skip(1)`-style tokens.
///
/// Accepts zero or one `--config PATH` occurrences (last wins). Returns
/// `Err` with a `harness: ...` message plus [`usage`] for: missing value,
/// any other `-`/`--` flag (including `--config=PATH`, `-c`, `--help`),
/// and any positional argument.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliArgs, String> {
    let mut cli = CliArgs::default();
    let mut iter = args.into_iter();
    while let Some(token) = iter.next() {
        if token == "--config" {
            match iter.next() {
                Some(path) => cli.config = Some(path),
                None => {
                    return Err(format!("harness: missing value for --config\n{}", usage()));
                }
            }
        } else if token.starts_with('-') {
            return Err(format!("harness: unknown flag '{token}'\n{}", usage()));
        } else {
            return Err(format!(
                "harness: unexpected argument '{token}'\n{}",
                usage()
            ));
        }
    }
    Ok(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn empty_means_default_config() {
        assert_eq!(parse_args(args(&[])).unwrap(), CliArgs { config: None });
    }

    #[test]
    fn config_takes_separate_value() {
        assert_eq!(
            parse_args(args(&["--config", "a.toml"])).unwrap(),
            CliArgs {
                config: Some("a.toml".to_owned())
            }
        );
    }

    #[test]
    fn repeat_is_last_wins() {
        assert_eq!(
            parse_args(args(&["--config", "a.toml", "--config", "b.toml"])).unwrap(),
            CliArgs {
                config: Some("b.toml".to_owned())
            }
        );
    }

    #[test]
    fn missing_value_is_error() {
        let err = parse_args(args(&["--config"])).unwrap_err();
        assert!(err.contains("missing value for --config"), "{err:?}");
        assert!(err.contains("Usage:"), "{err:?}");
    }

    #[test]
    fn equals_form_is_rejected() {
        let err = parse_args(args(&["--config=a.toml"])).unwrap_err();
        assert!(err.contains("unknown flag"), "{err:?}");
    }

    #[test]
    fn short_and_help_flags_are_rejected() {
        for flag in ["-c", "-h", "--help", "-V", "--version", "--foo"] {
            let err = parse_args(args(&[flag])).unwrap_err();
            assert!(err.contains("unknown flag"), "{flag}: {err:?}");
        }
    }

    #[test]
    fn positionals_are_rejected() {
        let err = parse_args(args(&["foo"])).unwrap_err();
        assert!(err.contains("unexpected argument"), "{err:?}");

        let err = parse_args(args(&["--config", "a.toml", "extra"])).unwrap_err();
        assert!(err.contains("unexpected argument"), "{err:?}");
    }
}
