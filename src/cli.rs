//! The command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::config::load::Overrides;

/// Shell type for the completion command — wraps `clap_complete::Shell` to
/// implement `ValueEnum` so clap can parse it from the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum CompletionShell {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
}

impl From<CompletionShell> for Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Elvish => Self::Elvish,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::PowerShell => Self::PowerShell,
            CompletionShell::Zsh => Self::Zsh,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "mrq",
    version,
    about = "A keyboard-driven TUI for the GitLab merge requests you need to review",
    long_about = None,
    // Subcommands are diagnostics; running bare `mrq` starts the TUI.
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Path to the configuration file (overrides $MRQ_CONFIG and the XDG location)
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Open on this filter instead of the first one
    #[arg(long, value_name = "NAME")]
    pub filter: Option<String>,

    /// Refresh interval in seconds for this run
    #[arg(long, value_name = "SECS")]
    pub refresh_secs: Option<u64>,

    /// Write the log here instead of the XDG state directory
    #[arg(long, value_name = "PATH", global = true)]
    pub log: Option<PathBuf>,

    /// Log verbosity: trace, debug, info, warn or error
    #[arg(long, value_name = "LEVEL", global = true)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Write a commented default configuration file
    InitConfig {
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
    },

    /// Validate the configuration, verify the token and check connectivity, then exit
    Check,

    /// Print the JSON Schema for the configuration file
    Schema,

    /// Generate a shell completion script and print it to stdout
    Completion {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
}

impl Cli {
    pub fn overrides(&self) -> Overrides {
        Overrides {
            refresh_secs: self.refresh_secs,
            filter: self.filter.clone(),
        }
    }

    /// The requested log level, and whether it parsed.
    ///
    /// Returns `Err` with the offending value so the caller can reject it before the
    /// terminal is taken over, rather than silently falling back to `info` and leaving
    /// the user wondering why `--log-level debug` did nothing.
    pub fn log_level(&self) -> Result<Option<tracing::Level>, &str> {
        match self.log_level.as_deref() {
            None => Ok(None),
            Some(raw) => crate::logging::parse_level(raw).map(Some).ok_or(raw),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("mrq").chain(args.iter().copied())).unwrap()
    }

    /// clap's own consistency checks: conflicting names, bad arg definitions.
    #[test]
    fn the_command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_starts_the_tui_with_no_overrides() {
        let cli = parse(&[]);
        assert_eq!(cli.command, None);
        assert_eq!(cli.overrides(), Overrides::default());
        assert_eq!(cli.log_level(), Ok(None));
    }

    #[test]
    fn every_documented_flag_parses() {
        let cli = parse(&[
            "--config",
            "/tmp/c.toml",
            "--filter",
            "Reviewing",
            "--refresh-secs",
            "45",
            "--log",
            "/tmp/l.log",
            "--log-level",
            "debug",
        ]);

        assert_eq!(cli.config, Some(PathBuf::from("/tmp/c.toml")));
        assert_eq!(cli.log, Some(PathBuf::from("/tmp/l.log")));
        assert_eq!(cli.log_level(), Ok(Some(tracing::Level::DEBUG)));
    }

    /// The flags must reach the config layer as overrides, not be read ad hoc.
    #[test]
    fn flags_become_config_overrides() {
        let cli = parse(&["--refresh-secs", "45", "--filter", "Mine"]);

        assert_eq!(
            cli.overrides(),
            Overrides {
                refresh_secs: Some(45),
                filter: Some("Mine".into()),
            }
        );
    }

    #[test]
    fn every_subcommand_is_registered() {
        assert_eq!(
            parse(&["init-config"]).command,
            Some(Command::InitConfig { force: false })
        );
        assert_eq!(
            parse(&["init-config", "--force"]).command,
            Some(Command::InitConfig { force: true })
        );
        assert_eq!(parse(&["check"]).command, Some(Command::Check));
        assert_eq!(parse(&["schema"]).command, Some(Command::Schema));
        assert_eq!(
            parse(&["completion", "zsh"]).command,
            Some(Command::Completion {
                shell: CompletionShell::Zsh
            })
        );
    }

    /// `--config` and the logging flags apply to the subcommands too: `mrq check
    /// --config other.toml` is the whole point of `check`.
    #[test]
    fn global_flags_work_with_subcommands() {
        let cli = parse(&["check", "--config", "/tmp/c.toml", "--log-level", "trace"]);
        assert_eq!(cli.command, Some(Command::Check));
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/c.toml")));
        assert_eq!(cli.log_level(), Ok(Some(tracing::Level::TRACE)));
    }

    /// A bad level is reported rather than silently falling back, or `--log-level debag`
    /// looks like logging is broken.
    #[test]
    fn an_unknown_log_level_is_reported_with_the_offending_value() {
        let cli = parse(&["--log-level", "verbose"]);
        assert_eq!(cli.log_level(), Err("verbose"));
    }

    #[test]
    fn unknown_flags_are_rejected() {
        assert!(Cli::try_parse_from(["mrq", "--nope"]).is_err());
        assert!(Cli::try_parse_from(["mrq", "not-a-subcommand"]).is_err());
    }

    #[test]
    fn refresh_secs_must_be_a_number() {
        assert!(Cli::try_parse_from(["mrq", "--refresh-secs", "soon"]).is_err());
    }

    #[test]
    fn version_is_available_and_matches_the_crate() {
        let rendered = Cli::command().render_version();
        assert!(rendered.contains(env!("CARGO_PKG_VERSION")), "{rendered}");
    }

    #[test]
    fn the_help_surface_is_pinned() {
        let help = Cli::command().render_long_help().to_string();

        for expected in [
            "--config",
            "--filter",
            "--refresh-secs",
            "--log",
            "--log-level",
            "init-config",
            "check",
            "schema",
            "completion",
        ] {
            assert!(help.contains(expected), "`{expected}` missing from --help");
        }

        // Pin the exact set. `-h, --help` and `-V, --version` render short-first, so
        // they are not picked up here and are covered by their own tests.
        let mut flags: Vec<&str> = help
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|w| w.starts_with("--"))
            .collect();
        flags.sort_unstable();
        flags.dedup();
        assert_eq!(
            flags,
            [
                "--config",
                "--filter",
                "--log",
                "--log-level",
                "--refresh-secs",
            ],
            "the flag set changed"
        );
    }
}
