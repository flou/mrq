//! OSC 52 clipboard writes, with native helpers for payloads too large for the escape.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// The largest single payload the OSC 52 escape carries, in text bytes.
///
/// Terminals enforce their own bounds on OSC payload length — the base64 step grows the
/// text by a third — and an oversized payload would come back as a truncated escape that
/// pastes garbage. Everything `mrq` copies (an MR URL, a branch name) is minuscule against
/// this bound, so it only guarantees the sequence is never clipped.
pub const OSC52_LIMIT_BYTES: usize = 64 * 1024;

/// `c` is the clipboard selection; `p` would be the X11 primary.
const SELECTION: char = 'c';

/// The native helpers visible on `$PATH`, injected so the choice stays a pure function.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Helpers {
    pub pbcopy: bool,
    pub wl_copy: bool,
    pub xclip: bool,
}

impl Helpers {
    pub fn detect() -> Self {
        Self {
            pbcopy: on_path("pbcopy"),
            wl_copy: on_path("wl-copy"),
            xclip: on_path("xclip"),
        }
    }
}

/// A decided copy: the write the caller has to perform.
#[derive(Debug, PartialEq, Eq)]
pub enum Copy {
    /// The escape bytes for stdout, to be written in a single call.
    Osc52(String),
    /// A helper program and its arguments, fed the text on stdin.
    Native(Vec<String>),
    /// Nothing on this host can carry it.
    Unavailable,
}

/// Decide how `text` reaches the clipboard: OSC 52 first, because the escape travels back
/// down the tty and so works over SSH; a payload over the bound would clip the sequence,
/// so it goes to the platform helper instead, or to `Unavailable` when there is none.
pub fn plan(text: &str, helpers: &Helpers) -> Copy {
    if text.len() <= OSC52_LIMIT_BYTES {
        return Copy::Osc52(escape(text));
    }
    match native(helpers) {
        Some(command) => Copy::Native(command),
        None => Copy::Unavailable,
    }
}

/// The full OSC 52 sequence. Nothing needs stripping here: the payload is base64 text, so
/// no `ESC`, `BEL` or newline can end the escape early the way one in a title could.
pub fn escape(text: &str) -> String {
    format!("\x1b]52;{SELECTION};{}\x1b\\", STANDARD.encode(text))
}

/// The platform helper for an oversized payload: `pbcopy` on macOS, then `wl-copy`, then
/// `xclip`, which needs its selection named.
fn native(helpers: &Helpers) -> Option<Vec<String>> {
    if cfg!(target_os = "macos") && helpers.pbcopy {
        Some(vec!["pbcopy".to_owned()])
    } else if cfg!(target_os = "linux") && helpers.wl_copy {
        Some(vec!["wl-copy".to_owned()])
    } else if cfg!(target_os = "linux") && helpers.xclip {
        Some(vec![
            "xclip".to_owned(),
            "-selection".to_owned(),
            "clipboard".to_owned(),
        ])
    } else {
        None
    }
}

#[derive(Debug)]
pub enum Error {
    /// The helper could not be started or fed.
    Io(std::io::Error),
    /// The helper ran but reported failure.
    NonZero(std::process::ExitStatus),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::NonZero(status) => write!(f, "the helper exited with {status}"),
        }
    }
}

impl std::error::Error for Error {}

/// Run a native helper with the text on stdin.
///
/// stdout and stderr are `/dev/null`: `xclip` and `wl-copy` keep a background process
/// alive to own the selection, and a pipe they still hold open would stall the wait below.
pub fn run_native(command: &[String], text: &str) -> Result<(), Error> {
    let (program, args) = command.split_first().ok_or_else(|| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty clipboard command",
        ))
    })?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(Error::Io)?;

    use std::io::Write;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Io(std::io::Error::other("the helper closed its stdin")))?;
    stdin.write_all(text.as_bytes()).map_err(Error::Io)?;
    drop(stdin);

    let status = child.wait().map_err(Error::Io)?;
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

    #[test]
    fn the_escape_is_base64_inside_an_osc_52_sequence() {
        let text = "https://gitlab.example.com/a/b/-/merge_requests/1";
        assert_eq!(
            escape(text),
            format!("\x1b]52;c;{}\x1b\\", STANDARD.encode(text))
        );
        assert!(escape(text).ends_with("\x1b\\"));
    }

    /// The base64 payload carries no `ESC`, `BEL` or newline, so hostile text cannot end
    /// the sequence early the way one could in a URL or a title.
    #[test]
    fn a_hostile_payload_cannot_escape_the_sequence() {
        let sequence = escape("boom\x1b]0;pwned\x07 and\r\nmore");
        let payload = sequence
            .trim_start_matches("\x1b]52;c;")
            .trim_end_matches("\x1b\\");
        assert!(!payload.contains('\x1b'));
        assert!(!payload.contains('\x07'));
        assert!(!payload.contains('\n'));
        assert!(!payload.contains('\r'));
    }

    /// The bound is inclusive: exactly `OSC52_LIMIT_BYTES` still rides OSC 52.
    #[test]
    fn the_bound_is_inclusive() {
        assert!(matches!(
            plan(&"x".repeat(OSC52_LIMIT_BYTES), &Helpers::default()),
            Copy::Osc52(_)
        ));
    }

    #[test]
    fn any_small_payload_is_osc_52_even_without_helpers() {
        assert!(matches!(plan("plain", &Helpers::default()), Copy::Osc52(_)));
    }

    #[test]
    fn an_oversized_payload_never_produces_a_truncated_escape() {
        let big = "x".repeat(OSC52_LIMIT_BYTES + 1);

        match plan(&big, &Helpers::default()) {
            Copy::Unavailable => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }

        let every_helper = Helpers {
            pbcopy: true,
            wl_copy: true,
            xclip: true,
        };
        match plan(&big, &every_helper) {
            Copy::Native(command) => assert!(!command.is_empty()),
            Copy::Unavailable => {} // no platform helper on this host
            Copy::Osc52(_) => panic!("an oversized payload produced an escape"),
        }
    }

    #[cfg(target_os = "macos")]
    mod on_macos {
        use super::*;

        #[test]
        fn pbcopy_is_the_first_choice() {
            assert_eq!(
                native(&Helpers {
                    pbcopy: true,
                    ..Helpers::default()
                }),
                Some(vec!["pbcopy".to_owned()])
            );
        }

        #[test]
        fn wayland_and_x11_helpers_do_not_apply_on_macos() {
            assert_eq!(
                native(&Helpers {
                    wl_copy: true,
                    xclip: true,
                    ..Helpers::default()
                }),
                None
            );
        }
    }

    #[cfg(target_os = "linux")]
    mod on_linux {
        use super::*;

        #[test]
        fn wl_copy_wins_over_xclip() {
            assert_eq!(
                native(&Helpers {
                    wl_copy: true,
                    xclip: true,
                    ..Helpers::default()
                }),
                Some(vec!["wl-copy".to_owned()])
            );
        }

        #[test]
        fn xclip_falls_back_and_names_its_selection() {
            assert_eq!(
                native(&Helpers {
                    xclip: true,
                    ..Helpers::default()
                }),
                Some(vec![
                    "xclip".to_owned(),
                    "-selection".to_owned(),
                    "clipboard".to_owned()
                ])
            );
        }

        #[test]
        fn no_helper_means_none() {
            assert_eq!(native(&Helpers::default()), None);
        }
    }

    #[test]
    fn the_text_reaches_the_helper_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("clipped");
        let command = vec!["tee".to_owned(), marker.display().to_string()];
        run_native(&command, "feature/42-fix").unwrap();
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "feature/42-fix");
    }

    #[test]
    fn a_failing_helper_reports_its_exit_status() {
        assert!(matches!(
            run_native(&["false".to_owned()], "x"),
            Err(Error::NonZero(_))
        ));
    }

    #[test]
    fn a_missing_helper_is_an_io_error() {
        assert!(matches!(
            run_native(&["mrq-definitely-not-a-real-binary".to_owned()], "x"),
            Err(Error::Io(_))
        ));
    }

    #[test]
    fn an_empty_command_is_rejected() {
        assert!(matches!(run_native(&[], "x"), Err(Error::Io(_))));
    }

    #[test]
    fn path_probing_does_not_pick_a_non_executable() {
        assert!(on_path("sh"));
        assert!(!on_path("mrq-definitely-not-a-real-binary"));
    }
}
