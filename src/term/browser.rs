//! Detached browser launching: the URL goes as a single argv element on a bare command.

use crate::config::schema::Browser;

/// A decided launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// A program and the arguments from a configured command, the URL appended last.
    Run(String, Vec<String>),
    /// Nothing on this host can open a browser.
    Unavailable,
}

/// The platform launcher visible on `$PATH`, injected so the choice stays a pure function.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Helpers {
    pub open: bool,
    pub xdg_open: bool,
}

impl Helpers {
    pub fn detect() -> Self {
        Self {
            open: on_path("open"),
            xdg_open: on_path("xdg-open"),
        }
    }
}

/// Decide how `url` opens: the configured command when set, else `open` on macOS and
/// `xdg-open` on Linux, else nothing to report.
///
/// The command is split on whitespace, never passed through a shell: the launcher and its
/// fixed arguments become the argv, and the URL is appended as one element.
pub fn plan(config: &Browser, helpers: &Helpers) -> Launch {
    let command: Vec<String> = config
        .command
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if let [program, args @ ..] = command.as_slice() {
        return Launch::Run(program.clone(), args.to_vec());
    }

    if cfg!(target_os = "macos") && helpers.open {
        return Launch::Run("open".to_owned(), Vec::new());
    }
    if cfg!(target_os = "linux") && helpers.xdg_open {
        return Launch::Run("xdg-open".to_owned(), Vec::new());
    }
    Launch::Unavailable
}

#[derive(Debug)]
pub enum Error {
    /// The launcher could not be started.
    Io(std::io::Error),
    /// The launcher ran but reported failure.
    NonZero(std::process::ExitStatus),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::NonZero(status) => write!(f, "the launcher exited with {status}"),
        }
    }
}

impl std::error::Error for Error {}

/// Launch the browser for `url`.
///
/// A launcher that inherited the screens' file descriptors could paint over the alternate
/// screen, so stdout and stderr are `/dev/null`. The wait is on the launcher itself —
/// `open` and `xdg-open` hand off to the real browser and exit — not on the browser.
pub fn run(url: &str, launch: &Launch) -> Result<(), Error> {
    let Launch::Run(program, args) = launch else {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no browser launcher is available",
        )));
    };
    let status = std::process::Command::new(program)
        .args(args)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(Error::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::NonZero(status))
    }
}

/// Whether an executable of this name is on `$PATH`.
fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        std::fs::metadata(&candidate).is_ok_and(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Browser {
        Browser::default()
    }

    // ------------------------------------------------------------------ the decision

    #[test]
    fn an_explicit_command_wins_over_the_platform_default() {
        let mut browser = config();
        browser.command = "firefox --new-window".into();
        let helpers = Helpers {
            open: true,
            xdg_open: true,
        };
        assert_eq!(
            plan(&browser, &helpers),
            Launch::Run("firefox".into(), vec!["--new-window".into()])
        );
    }

    #[test]
    fn command_whitespace_is_treated_literally() {
        let mut browser = config();
        browser.command = "open -a \"Safari\"".into();
        assert_eq!(
            plan(&browser, &Helpers::default()),
            Launch::Run("open".into(), vec!["-a".into(), "\"Safari\"".into()]),
            "quotes are not shell-interpreted; the URL is one argv element either way"
        );
    }

    #[cfg(target_os = "macos")]
    mod on_macos {
        use super::*;

        #[test]
        fn a_blank_command_defaults_to_open() {
            assert_eq!(
                plan(
                    &config(),
                    &Helpers {
                        open: true,
                        ..Helpers::default()
                    }
                ),
                Launch::Run("open".into(), Vec::new())
            );
        }

        #[test]
        fn xdg_open_does_not_apply_on_macos() {
            assert_eq!(
                plan(
                    &config(),
                    &Helpers {
                        xdg_open: true,
                        ..Helpers::default()
                    }
                ),
                Launch::Unavailable
            );
        }
    }

    #[cfg(target_os = "linux")]
    mod on_linux {
        use super::*;

        #[test]
        fn a_blank_command_defaults_to_xdg_open() {
            assert_eq!(
                plan(
                    &config(),
                    &Helpers {
                        xdg_open: true,
                        ..Helpers::default()
                    }
                ),
                Launch::Run("xdg-open".into(), Vec::new())
            );
        }

        #[test]
        fn open_does_not_apply_on_linux() {
            assert_eq!(
                plan(
                    &config(),
                    &Helpers {
                        open: true,
                        ..Helpers::default()
                    }
                ),
                Launch::Unavailable
            );
        }
    }

    #[test]
    fn no_launcher_on_this_platform_is_unavailable() {
        assert_eq!(plan(&config(), &Helpers::default()), Launch::Unavailable);
    }

    // ------------------------------------------------------------------ the launch

    #[test]
    fn a_launcher_that_succeeds_is_ok() {
        let launch = Launch::Run("true".into(), Vec::new());
        run("https://gitlab.example.com/a/b/-/merge_requests/1", &launch).unwrap();
    }

    #[test]
    fn a_failing_launcher_reports_its_exit_status() {
        let launch = Launch::Run("false".into(), Vec::new());
        assert!(matches!(run("https://x", &launch), Err(Error::NonZero(_))));
    }

    #[test]
    fn a_missing_launcher_is_an_io_error() {
        let launch = Launch::Run("mrq-definitely-not-a-real-binary".into(), Vec::new());
        assert!(matches!(run("https://x", &launch), Err(Error::Io(_))));
    }

    #[test]
    fn launching_without_a_plan_is_an_error() {
        assert!(matches!(
            run("https://x", &Launch::Unavailable),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn path_probing_does_not_pick_a_non_executable() {
        assert!(on_path("sh"));
        assert!(!on_path("mrq-definitely-not-a-real-binary"));
    }
}
