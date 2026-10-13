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
///
/// The diff is staged in an anonymous file — unlinked from the start, so nothing is left
/// in `$TMPDIR` however mrq exits. On a terminal the viewer runs as its own foreground
/// process group, as a shell would run it: `ctrl-c`, `ctrl-\` and `ctrl-z` go to the viewer
/// alone instead of to mrq as well.
pub fn run(command: &str, diff: &str) -> Result<(), Error> {
    run_with(
        command,
        diff,
        std::io::IsTerminal::is_terminal(&std::io::stdin()),
    )
}

/// [`run`], with whether stdin is a terminal injected: a test must not hand the terminal
/// of whoever runs `cargo test` to its child.
fn run_with(command: &str, diff: &str, on_terminal: bool) -> Result<(), Error> {
    use std::io::Seek as _;
    use std::os::unix::process::CommandExt as _;

    let mut staged = tempfile::tempfile().map_err(Error::Io)?;
    staged.write_all(diff.as_bytes()).map_err(Error::Io)?;
    staged.rewind().map_err(Error::Io)?;

    let mut child = std::process::Command::new("sh");
    child.arg("-c").arg(command).stdin(staged);
    if on_terminal {
        child.process_group(0);
    }
    let mut child = child.spawn().map_err(Error::Io)?;

    let status = if on_terminal {
        foreground::wait(&child)
    } else {
        child.wait()
    }
    .map_err(Error::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::NonZero(status))
    }
}

/// Lending the terminal to a child's process group, and taking it back.
mod foreground {
    use std::io;
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{Child, ExitStatus};

    use nix::errno::Errno;
    use nix::sys::signal::{SigSet, SigmaskHow, Signal, killpg, pthread_sigmask};
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::{Pid, getpgrp, tcsetpgrp};

    /// Make `child`'s group the terminal's foreground, wait for it to exit, and make mrq's
    /// group the foreground again.
    pub fn wait(child: &Child) -> io::Result<ExitStatus> {
        let group = Pid::from_raw(i32::try_from(child.id()).map_err(io::Error::other)?);
        let tty = io::stdin();

        // Taking the terminal back happens from what is then a background group, which
        // the kernel answers with SIGTTOU unless the signal is blocked.
        let _ttou = BlockTtou::new();
        let handed = tcsetpgrp(&tty, group).is_ok();
        // The child may have touched the terminal before it was handed over, and been
        // stopped for it; it has the terminal now, so let it carry on.
        let _ = killpg(group, Signal::SIGCONT);

        let status = loop {
            match waitpid(group, Some(WaitPidFlag::WUNTRACED)) {
                Ok(WaitStatus::Exited(_, code)) => break Ok(ExitStatus::from_raw(code << 8)),
                Ok(WaitStatus::Signaled(_, signal, _)) => {
                    break Ok(ExitStatus::from_raw(signal as i32));
                }
                // ctrl-z: there is no shell job to return to, only mrq, which is waiting
                // for this very process, so the stop is undone rather than left to hang.
                // A child that could not be given the terminal stops on every read, so
                // that one is ended instead.
                Ok(WaitStatus::Stopped(..)) => {
                    if !handed {
                        let _ = killpg(group, Signal::SIGTERM);
                    }
                    let _ = killpg(group, Signal::SIGCONT);
                }
                Ok(_) | Err(Errno::EINTR) => {}
                Err(errno) => break Err(io::Error::from(errno)),
            }
        };

        if handed {
            let _ = tcsetpgrp(&tty, getpgrp());
        }
        status
    }

    /// Blocks SIGTTOU on this thread for as long as it lives.
    struct BlockTtou(Option<SigSet>);

    impl BlockTtou {
        fn new() -> Self {
            let mut set = SigSet::empty();
            set.add(Signal::SIGTTOU);
            let mut previous = SigSet::empty();
            let blocked =
                pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), Some(&mut previous)).is_ok();
            Self(blocked.then_some(previous))
        }
    }

    impl Drop for BlockTtou {
        fn drop(&mut self) {
            if let Some(previous) = &self.0 {
                let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(previous), None);
            }
        }
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

        run_with(&command, "+added\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(out).unwrap(), "+added\n");
    }

    #[test]
    fn a_failing_viewer_is_reported() {
        assert!(matches!(
            run_with("exit 3", "", false),
            Err(Error::NonZero(_))
        ));
    }
}
