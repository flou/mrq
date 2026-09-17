//! GitLab personal access token resolution and the redacting wrapper.
//!
//! The token is the one genuinely sensitive value in the process, so its
//! handling is concentrated here: four sources in a fixed precedence, a subprocess
//! escape hatch for keychain tools, and a type that cannot be printed by accident.
//!
//! The resolver takes its environment as an argument for the same reason
//! [`super::paths`] does — tests that mutate the process environment race each other
//! when the harness runs them as threads.

use std::fmt;
use std::time::Duration;

use crate::error::ConfigError;

/// A personal access token.
///
/// `Debug` and `Display` both redact. That is the entire point of the type: the token
/// must never reach the log or the cache, and the realistic way that happens is a
/// `tracing::debug!("{config:?}")` added months from now by someone who has not read
/// this doc comment. Redacting at the type makes that safe by construction rather than
/// by remembering.
///
/// There is deliberately no `Serialize` impl, so a token cannot be written into a cache
/// file even intentionally.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    value: String,
    source: TokenSource,
}

impl Token {
    const fn new(value: String, source: TokenSource) -> Self {
        Self { value, source }
    }

    /// The token itself. The only caller should be the code building the `Authorization`
    /// header; the name is deliberately blunt so its use stands out in review.
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// Where this token came from, named in the auth-failure message.
    pub(crate) const fn source(&self) -> TokenSource {
        self.source
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Token(<redacted>, from {})", self.source.as_str())
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Which of the four sources supplied the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    MrqTokenEnv,
    GitlabTokenEnv,
    TokenCommand,
    ConfigFile,
}

impl TokenSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MrqTokenEnv => "$MRQ_TOKEN",
            Self::GitlabTokenEnv => "$GITLAB_TOKEN",
            Self::TokenCommand => "token_command",
            Self::ConfigFile => "the config file",
        }
    }
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The environment variables consulted during token resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenEnv {
    pub mrq_token: Option<String>,
    pub gitlab_token: Option<String>,
}

impl TokenEnv {
    pub fn from_process() -> Self {
        // An empty variable is treated as unset. `export MRQ_TOKEN=` is how people
        // unset one in a shell profile, and an empty Bearer header fails with a 401
        // that gives no hint about the real cause.
        let var = |k| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self {
            mrq_token: var("MRQ_TOKEN"),
            gitlab_token: var("GITLAB_TOKEN"),
        }
    }
}

/// Warnings raised during resolution that are not fatal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenWarning {
    /// A literal token in a file others can read.
    PermissivePermissions { path: String, mode: u32 },
}

impl fmt::Display for TokenWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissivePermissions { path, mode } => write!(
                f,
                "{path} contains a literal token and is readable by others (mode {mode:04o}); \
                 consider `chmod 600 {path}`, or use token_command instead"
            ),
        }
    }
}

/// A resolved token plus any non-fatal warnings about how it was obtained.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub token: Token,
    pub warnings: Vec<TokenWarning>,
}

/// Resolve the token from the four sources, in precedence order.
///
/// `config_path` is the file the literal `token` came from, used only to check its
/// permissions; pass `None` when the configuration did not come from a file.
pub fn resolve(
    gitlab: &super::schema::Gitlab,
    env: &TokenEnv,
    config_path: Option<&std::path::Path>,
) -> Result<Resolved, ConfigError> {
    let mut warnings = Vec::new();

    // 1. $MRQ_TOKEN
    if let Some(value) = clean(env.mrq_token.as_deref()) {
        return Ok(Resolved {
            token: Token::new(value, TokenSource::MrqTokenEnv),
            warnings,
        });
    }

    // 2. $GITLAB_TOKEN
    if let Some(value) = clean(env.gitlab_token.as_deref()) {
        return Ok(Resolved {
            token: Token::new(value, TokenSource::GitlabTokenEnv),
            warnings,
        });
    }

    // 3. token_command
    if let Some(command) = gitlab.token_command.as_deref().filter(|c| !c.is_empty()) {
        let value = run_token_command(command)?;
        return Ok(Resolved {
            token: Token::new(value, TokenSource::TokenCommand),
            warnings,
        });
    }

    // 4. a literal `token` in the config file
    if let Some(value) = clean(gitlab.token.as_deref()) {
        if let Some(path) = config_path
            && let Some(mode) = world_or_group_readable(path)
        {
            warnings.push(TokenWarning::PermissivePermissions {
                path: path.display().to_string(),
                mode,
            });
        }
        return Ok(Resolved {
            token: Token::new(value, TokenSource::ConfigFile),
            warnings,
        });
    }

    Err(ConfigError::MissingToken)
}

/// Trim and reject blank values.
///
/// Tokens get copied out of a browser or a password manager, and a trailing newline is
/// the most common way that goes wrong. It produces an invalid header rather than a
/// clear error, so it is worth stripping here.
fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// How long `token_command` gets to produce a token.
///
/// This runs before the alternate screen is entered, so an unbounded wait shows the user
/// nothing at all — not a spinner, not an error, just a shell that never comes back.
///
/// Ten seconds rather than the "couple" a hung process deserves, because the command is
/// expected to be interactive: a `gpg` pinentry passphrase or a Touch ID prompt is a
/// legitimate reason for it to take a while, and killing one of those would break a
/// working setup to fix a broken one. Long enough to answer a prompt, short enough that a
/// genuinely stuck helper reads as an error rather than a freeze.
pub const TOKEN_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the wait checks whether the child has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Run `token_command` through the shell and take its stdout.
///
/// The shell is intentional: users will write commands like
/// `security find-generic-password -w` and `op read`, pipelines with quoting and pipes
/// in them.
fn run_token_command(command: &str) -> Result<String, ConfigError> {
    run_token_command_within(command, TOKEN_COMMAND_TIMEOUT)
}

/// The body of [`run_token_command`], with the deadline injected so the timeout path is
/// testable without a ten-second test.
fn run_token_command_within(command: &str, timeout: Duration) -> Result<String, ConfigError> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    // Only the program name reaches the error — never the full command, which commonly
    // carries a secret as an argument (see `ConfigError::TokenCommand`'s doc).
    let program = command.split_whitespace().next().unwrap_or(command);
    let failed = |reason: String| ConfigError::TokenCommand {
        program: program.to_owned(),
        reason,
    };

    // stdin is deliberately inherited. A pinentry that needs a TTY is a supported setup,
    // and nothing has taken the terminal over yet.
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(e.to_string()))?;

    // Polled rather than waited on, because `wait` and `output` have no deadline and this
    // module cannot reach for tokio: it runs before the runtime is built.
    //
    // The pipes are therefore not drained until the child exits, so a command that writes
    // more than a pipe buffer holds would block on the write and be reported as a
    // timeout. A token is one line; a `token_command` emitting 64KB is already misused,
    // and "did not finish" is the right diagnosis for it either way.
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                // Best effort: this kills the shell, and a grandchild holding the terminal
                // may outlive it. Reaped so it does not become a zombie for the rest of
                // the session.
                let _ = child.kill();
                let _ = child.wait();
                return Err(failed(format!(
                    "did not finish within {}s. If it prompts for input, answer it before \
                     starting mrq, or use a command that does not prompt",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => return Err(failed(e.to_string())),
        }
    };

    let read = |pipe: Option<&mut dyn Read>| {
        let mut buf = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    };
    let stdout = read(child.stdout.as_mut().map(|p| p as &mut dyn Read));
    let stderr = read(child.stderr.as_mut().map(|p| p as &mut dyn Read));

    if !status.success() {
        // Stderr goes to the message and to the log: a keychain prompt that was declined
        // or a missing entry is the usual cause, and the tool's own words say it best.
        let detail = stderr.trim();
        return Err(failed(match (status.code(), detail.is_empty()) {
            (Some(code), true) => format!("exited with status {code}"),
            (Some(code), false) => format!("exited with status {code}: {detail}"),
            (None, true) => "killed by a signal".to_owned(),
            (None, false) => format!("killed by a signal: {detail}"),
        }));
    }

    clean(Some(&stdout)).ok_or_else(|| failed("produced no output".to_owned()))
}

/// The file mode if it is readable by group or other, otherwise `None`.
fn world_or_group_readable(path: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then_some(mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Gitlab;

    fn gitlab() -> Gitlab {
        Gitlab::default()
    }

    fn env(mrq: Option<&str>, gl: Option<&str>) -> TokenEnv {
        TokenEnv {
            mrq_token: mrq.map(str::to_owned),
            gitlab_token: gl.map(str::to_owned),
        }
    }

    /// The reason the type exists: a token must survive contact with a debug formatter.
    #[test]
    fn token_is_redacted_by_every_formatter() {
        let secret = "glpat-SUPERSECRET";
        let token = Token::new(secret.into(), TokenSource::MrqTokenEnv);

        for rendered in [
            format!("{token:?}"),
            format!("{token}"),
            format!("{token:#?}"),
        ] {
            assert!(
                !rendered.contains(secret),
                "token leaked through a formatter: {rendered}"
            );
            assert!(rendered.contains("redacted"), "{rendered}");
        }
        assert_eq!(token.expose(), secret, "but it is still usable");
    }

    /// A token reaches the log via a struct that contains it, not usually on its own.
    #[test]
    fn token_is_redacted_inside_a_containing_struct() {
        #[derive(Debug)]
        struct Holder {
            // Never read directly: the test is that Debug alone redacts it.
            #[allow(dead_code)]
            token: Token,
        }
        let holder = Holder {
            token: Token::new("glpat-LEAKME".into(), TokenSource::ConfigFile),
        };
        let rendered = format!("{holder:?}");
        assert!(!rendered.contains("LEAKME"), "{rendered}");
    }

    #[test]
    fn precedence_is_mrq_then_gitlab_then_command_then_file() {
        let mut g = gitlab();
        g.token = Some("from-file".into());
        g.token_command = Some("printf from-command".into());

        let all = env(Some("from-mrq"), Some("from-gitlab"));
        assert_eq!(resolve(&g, &all, None).unwrap().token.expose(), "from-mrq");

        let no_mrq = env(None, Some("from-gitlab"));
        let got = resolve(&g, &no_mrq, None).unwrap();
        assert_eq!(got.token.expose(), "from-gitlab");
        assert_eq!(got.token.source(), TokenSource::GitlabTokenEnv);

        let got = resolve(&g, &TokenEnv::default(), None).unwrap();
        assert_eq!(got.token.expose(), "from-command");
        assert_eq!(got.token.source(), TokenSource::TokenCommand);

        g.token_command = None;
        let got = resolve(&g, &TokenEnv::default(), None).unwrap();
        assert_eq!(got.token.expose(), "from-file");
        assert_eq!(got.token.source(), TokenSource::ConfigFile);
    }

    /// A missing token is fatal, and the message names all four sources so
    /// a user who set the wrong variable can see which one mrq actually reads.
    #[test]
    fn missing_token_is_fatal_and_names_every_source() {
        let err = resolve(&gitlab(), &TokenEnv::default(), None).unwrap_err();
        assert!(matches!(err, ConfigError::MissingToken));

        let msg = err.to_string();
        for source in ["MRQ_TOKEN", "GITLAB_TOKEN", "token_command", "token"] {
            assert!(msg.contains(source), "missing `{source}` in: {msg}");
        }
    }

    /// Tokens are pasted from browsers and password managers; a trailing newline would
    /// otherwise produce an invalid header and a 401 that explains nothing.
    #[test]
    fn surrounding_whitespace_is_stripped_from_every_source() {
        let got = resolve(&gitlab(), &env(Some("  glpat-x\n"), None), None).unwrap();
        assert_eq!(got.token.expose(), "glpat-x");

        let mut g = gitlab();
        g.token_command = Some("printf '  glpat-y\n\n'".into());
        let got = resolve(&g, &TokenEnv::default(), None).unwrap();
        assert_eq!(got.token.expose(), "glpat-y");
    }

    #[test]
    fn blank_values_are_treated_as_unset() {
        let mut g = gitlab();
        g.token = Some("from-file".into());

        // An empty variable must fall through, not produce an empty Bearer header.
        let got = resolve(&g, &env(Some("   "), Some("\n")), None).unwrap();
        assert_eq!(got.token.expose(), "from-file");
    }

    #[test]
    fn a_failing_token_command_is_fatal_and_quotes_stderr() {
        let mut g = gitlab();
        g.token_command = Some("echo 'no such keychain item' >&2; exit 3".into());

        let err = resolve(&g, &TokenEnv::default(), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("status 3"), "{msg}");
        assert!(msg.contains("no such keychain item"), "{msg}");
    }

    /// A command that succeeds but prints nothing is a misconfiguration, not an empty
    /// token to send to GitLab.
    #[test]
    fn a_silent_token_command_is_an_error() {
        let mut g = gitlab();
        g.token_command = Some("true".into());

        let err = resolve(&g, &TokenEnv::default(), None).unwrap_err();
        assert!(err.to_string().contains("no output"), "{err}");
    }

    #[test]
    fn token_command_runs_through_a_shell() {
        let mut g = gitlab();
        // Pipes and quoting must work — real commands look like `op read` and `pass show`.
        g.token_command = Some("printf 'a-b-c' | tr -d '-'".into());

        let got = resolve(&g, &TokenEnv::default(), None).unwrap();
        assert_eq!(got.token.expose(), "abc");
    }

    /// The bug this guards: an unbounded wait leaves the user with no output at all —
    /// mrq has not drawn anything yet, so a hung helper is indistinguishable from a hang
    /// in mrq itself.
    #[test]
    fn a_hanging_token_command_times_out_rather_than_blocking_startup() {
        let started = std::time::Instant::now();
        let err = run_token_command_within("sleep 45", Duration::from_millis(150)).unwrap_err();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?}; the deadline was not honoured",
            started.elapsed()
        );

        let msg = err.to_string();
        assert!(msg.contains("sleep"), "should name the program: {msg}");
        assert!(
            !msg.contains("sleep 45"),
            "should not repeat the command's arguments: {msg}"
        );
        assert!(msg.contains("did not finish"), "{msg}");
        assert!(
            msg.contains("prompt"),
            "should say what to do about it: {msg}"
        );
    }

    /// The timeout must not truncate a command that is merely slow, or a pinentry
    /// passphrase and a Touch ID prompt become unusable.
    #[test]
    fn a_slow_but_finishing_token_command_still_succeeds() {
        let got = run_token_command_within("sleep 0.2; printf glpat-slow", TOKEN_COMMAND_TIMEOUT)
            .unwrap();
        assert_eq!(got, "glpat-slow");
    }

    /// A killed child must be reaped, or it lingers as a zombie for the whole session.
    ///
    /// `std::process::Child` does not reap on drop, so dropping a killed child without
    /// waiting leaves one — which is why the timeout path waits explicitly.
    #[test]
    fn a_timed_out_command_leaves_no_zombie() {
        run_token_command_within("sleep 30", Duration::from_millis(50)).unwrap_err();

        // Polled: SIGKILL delivery and the transition to Z are asynchronous, so an
        // immediate single look would pass whether or not the child was reaped.
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            assert_eq!(own_zombies("sleep 30"), 0, "a zombie survived the timeout");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The number of zombie processes this test binary left for one specific command.
    ///
    /// The timeout tests run in parallel in the same process, and each kills its own
    /// child; a poll that caught one of theirs (killed but not yet reaped) would misread
    /// it as ours. Scoping by a unique command marker keeps each test's zombies distinct.
    fn own_zombies(command_marker: &str) -> usize {
        let ps = std::process::Command::new("ps")
            .args(["-A", "-o", "stat=,ppid=,command="])
            .output()
            .expect("ps is available on macOS and Linux");

        let me = std::process::id().to_string();
        String::from_utf8_lossy(&ps.stdout)
            .lines()
            .filter(|line| {
                let mut fields = line.split_whitespace();
                let Some(stat) = fields.next() else {
                    return false;
                };
                let Some(ppid) = fields.next() else {
                    return false;
                };
                stat.starts_with('Z') && ppid == me && line.contains(command_marker)
            })
            .count()
    }

    /// The deadline applies to the shell, not to each stage of a pipeline, so a slow
    /// stage cannot buy the whole command more time than it is allowed.
    #[test]
    fn the_deadline_covers_the_whole_pipeline() {
        let err = run_token_command_within(
            "printf glpat-x | (sleep 40; cat)",
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err}");
    }

    #[test]
    fn a_missing_token_command_binary_is_reported_not_panicked() {
        let mut g = gitlab();
        g.token_command = Some("mrq-definitely-not-a-real-binary".into());

        let err = resolve(&g, &TokenEnv::default(), None).unwrap_err();
        assert!(matches!(err, ConfigError::TokenCommand { .. }), "{err:?}");
    }

    /// A `token_command` failure must not echo the command's own arguments: a user is as
    /// likely to write `curl -H 'Authorization: Bearer <secret>' ...` as anything else,
    /// and this error is both printed to stderr and written to the log file on the fatal
    /// path (`main.rs`). Only the program name may appear.
    ///
    /// The command here fails silently (empty stderr) so this isolates the command-line
    /// redaction from the separate, deliberate choice to embed a helper's own stderr —
    /// that is `logging.rs`'s and `error.rs`'s concern, not this one's.
    #[test]
    fn a_failing_token_command_does_not_leak_its_arguments() {
        let err = run_token_command_within(
            "false --token=not-a-real-secret-abc123",
            TOKEN_COMMAND_TIMEOUT,
        )
        .unwrap_err();

        let rendered = err.to_string();
        assert!(
            !rendered.contains("not-a-real-secret-abc123"),
            "the command's arguments must not appear in the error: {rendered}"
        );
        assert!(
            rendered.contains("false"),
            "the program name should still appear: {rendered}"
        );
    }

    #[test]
    fn a_group_readable_config_with_a_literal_token_warns_but_continues() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut g = gitlab();
        g.token = Some("glpat-x".into());
        let got = resolve(&g, &TokenEnv::default(), Some(&path)).unwrap();

        assert_eq!(got.token.expose(), "glpat-x", "resolution still succeeds");
        assert_eq!(got.warnings.len(), 1);
        let rendered = got.warnings[0].to_string();
        assert!(rendered.contains("0644"), "{rendered}");
        assert!(!rendered.contains("glpat-x"), "warning leaked the token");
    }

    #[test]
    fn a_private_config_produces_no_warning() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut g = gitlab();
        g.token = Some("glpat-x".into());
        let got = resolve(&g, &TokenEnv::default(), Some(&path)).unwrap();
        assert!(got.warnings.is_empty());
    }

    /// The permission warning is specific to a literal token in the file. A token from
    /// the environment or a keychain is not exposed by the file's mode.
    #[test]
    fn permissions_are_not_checked_when_the_token_came_from_elsewhere() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        let got = resolve(&gitlab(), &env(Some("glpat-env"), None), Some(&path)).unwrap();
        assert!(got.warnings.is_empty());
    }
}
