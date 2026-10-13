//! The diff viewer: a shell command that owns the terminal while it reads a diff.
//!
//! Unlike the browser launch, the viewer is interactive — it inherits the terminal, so
//! the caller must have suspended the TUI first (see `guard::suspend`). The diff arrives
//! on stdin from a file rather than a pipe, so a pager can seek in it and a large diff
//! never blocks on a full pipe buffer.

use std::io::Write as _;

use crate::config::schema::Diff;

/// The fallback when `delta` is installed. `--paging=always` because delta otherwise
/// decides from the diff's height, and a short diff would flash past and return at once.
const DELTA: &str = "delta --paging=always";

/// The fallback when nothing better is installed. `-R` keeps colour escapes intact.
const LESS: &str = "less -R";

/// Which viewers are visible on `$PATH`, injected so the choice stays a pure function.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Helpers {
    pub delta: bool,
}

impl Helpers {
    pub fn detect() -> Self {
        Self {
            delta: crate::term::browser::on_path("delta"),
        }
    }
}

/// The shell command to run: the configured one when set, else delta, else less.
pub fn plan(config: &Diff, helpers: &Helpers) -> String {
    let configured = config.command.trim();
    if !configured.is_empty() {
        return configured.to_owned();
    }
    if helpers.delta { DELTA } else { LESS }.to_owned()
}

#[derive(Debug)]
pub enum Error {
    /// The diff could not be staged or the viewer could not be started.
    Io(std::io::Error),
    /// The viewer ran but reported failure.
    NonZero(std::process::ExitStatus),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::NonZero(status) => write!(f, "the viewer exited with {status}"),
        }
    }
}

impl std::error::Error for Error {}

/// Run `command` through `sh -c` with `diff` on stdin, and wait for it.
///
/// Blocking: the terminal belongs to the viewer until it exits, and nothing else may read
/// or paint in the meantime.
pub fn run(command: &str, diff: &str) -> Result<(), Error> {
    let mut staged = tempfile::Builder::new()
        .prefix("mrq-")
        .suffix(".diff")
        .tempfile()
        .map_err(Error::Io)?;
    staged.write_all(diff.as_bytes()).map_err(Error::Io)?;
    staged.flush().map_err(Error::Io)?;
    let stdin = staged.reopen().map_err(Error::Io)?;

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(stdin)
        .status()
        .map_err(Error::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::NonZero(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(command: &str) -> Diff {
        Diff {
            command: command.to_owned(),
        }
    }

    #[test]
    fn a_configured_command_wins() {
        let helpers = Helpers { delta: true };
        assert_eq!(plan(&configured("riff"), &helpers), "riff");
        assert_eq!(
            plan(&configured("  delta --side-by-side "), &helpers),
            "delta --side-by-side"
        );
    }

    #[test]
    fn delta_is_the_default_when_installed() {
        assert_eq!(
            plan(&Diff::default(), &Helpers { delta: true }),
            "delta --paging=always"
        );
    }

    #[test]
    fn less_is_the_last_resort() {
        assert_eq!(plan(&Diff::default(), &Helpers::default()), "less -R");
        assert_eq!(plan(&configured("   "), &Helpers::default()), "less -R");
    }

    #[test]
    fn the_viewer_reads_the_diff_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let command = format!("cat > '{}'", out.display());

        run(&command, "+added\n").unwrap();
        assert_eq!(std::fs::read_to_string(out).unwrap(), "+added\n");
    }

    #[test]
    fn a_failing_viewer_is_reported() {
        assert!(matches!(run("exit 3", ""), Err(Error::NonZero(_))));
    }
}
