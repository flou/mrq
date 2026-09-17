//! The error taxonomy and the process exit codes.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crate::config::token::TokenSource;

/// Where in the process lifetime a failure happened.
///
/// Some failures are fatal before the terminal is taken over but survivable afterwards —
/// a 401 at startup means the token is wrong and there is nothing to show, while a 401
/// mid-session means the token was revoked and the last snapshot is still worth keeping
/// on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before the alternate screen is entered.
    Startup,
    /// While the TUI owns the terminal.
    Runtime,
}

/// What the application should do about a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Report and exit. The terminal has not been taken over yet, or cannot be kept.
    Fatal,
    /// Stop refreshing every filter and keep the last snapshot on screen. Used when
    /// retrying would only hammer the instance with a credential it has already refused.
    PauseRefreshes,
    /// Retry this filter later, keeping the last snapshot. `retry_after` is the server's
    /// instruction when it sent one, otherwise the caller applies its own backoff.
    Backoff { retry_after: Option<Duration> },
    /// Render the data that did arrive and mark the tab. Used for partial GraphQL
    /// responses, which carry both `data` and `errors`.
    RenderPartial,
    /// Retry once with the reduced fragment — premium fields stripped.
    ReduceFragment,
    /// Walk the complexity ladder: halve the page size, then fall back to the reduced
    /// fragment.
    DegradeComplexity,
}

/// Configuration and credential failures. Always fatal, always exit code 2.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A value that parsed but is not usable — an interval below the floor, a label
    /// filter on a currentUser-rooted scope, an unknown column key.
    #[error("{key}: {message}")]
    Invalid { key: String, message: String },

    /// A binding claimed by two actions is a startup error, because silently letting
    /// one win makes the help popup lie.
    #[error("key `{key}` is bound to both `{first}` and `{second}`")]
    DuplicateKeybind {
        key: String,
        first: String,
        second: String,
    },

    #[error("`{spec}` is not a valid key specification: {reason}")]
    KeySpec { spec: String, reason: String },

    #[error(
        "no GitLab token found. Set $MRQ_TOKEN or $GITLAB_TOKEN, or add `token_command` \
         or `token` to the [gitlab] section of your config file"
    )]
    MissingToken,

    /// `program` is only the command's first whitespace-delimited token, not the whole
    /// `token_command` line: that line commonly carries a secret as an argument (a bearer
    /// token, an inline passphrase), and this error is both printed to stderr and written
    /// to the log file on the fatal path.
    #[error("token_command (`{program}` ...) failed: {reason}")]
    TokenCommand { program: String, reason: String },

    /// Neither `$HOME` nor the relevant `XDG_*` variable is set, so there is no way to
    /// locate the config, cache or state directory.
    #[error(
        "cannot locate the mrq directories: $HOME is not set. Set $HOME, or set \
         $XDG_CONFIG_HOME, $XDG_CACHE_HOME and $XDG_STATE_HOME explicitly"
    )]
    HomeNotFound,
}

/// A single error entry from a GraphQL response body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphQlError {
    pub message: String,
    /// Dotted response path the error applies to, when the server supplied one.
    pub path: Option<String>,
    /// `extensions.code`, e.g. `undefinedField`. Classification prefers this over
    /// matching on the message, which is prose and changes between GitLab releases.
    pub code: Option<String>,
    /// `extensions.fieldName`. The field the server rejected, which is what lets the
    /// retry drop exactly that field instead of a whole category.
    pub field: Option<String>,
}

impl GraphQlError {
    #[cfg(test)]
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }
}

/// Whether the server rejected a document for breadth or for nesting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Complexity,
    Depth,
}

impl LimitKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complexity => "complexity",
            Self::Depth => "depth",
        }
    }
}

/// Everything that can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// HTTP 401 or 403. The token is missing a scope, expired, revoked — or valid for a
    /// different instance than the one `gitlab.url` points at.
    #[error(
        "{instance} rejected the token from {token_source} (HTTP {status}) — check that \
         it is valid and has the `read_api` scope, and that gitlab.url names the right \
         instance"
    )]
    Unauthorized {
        status: u16,
        instance: String,
        token_source: TokenSource,
    },

    /// The instance returned HTTP 200 for `currentUser` but resolved it to `null`. The
    /// token authenticated — it just has no user attached, which is what a revoked or
    /// project-scoped token looks like. Distinct from [`Self::Unauthorized`] because the
    /// instance did not reject anything.
    #[error(
        "{instance} accepted the token from {token_source} but it resolved to no user — \
         it may be revoked or project-scoped"
    )]
    NoCurrentUser {
        instance: String,
        token_source: TokenSource,
    },

    /// HTTP 429, or any response carrying `Retry-After`.
    #[error("rate limited by GitLab{}", match .retry_after {
        Some(d) => format!(" — retrying in {}s", d.as_secs()),
        None => String::new(),
    })]
    RateLimited { retry_after: Option<Duration> },

    /// A 5xx, or any other unexpected status.
    #[error("GitLab returned HTTP {status}")]
    Http { status: u16 },

    /// Connection failure, TLS failure or timeout.
    #[error("could not reach GitLab: {0}")]
    Network(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// The response carried both `data` and `errors`. Some of the snapshot is usable.
    #[error("GitLab returned {} error(s) alongside partial data", .errors.len())]
    GraphQlPartial { errors: Vec<GraphQlError> },

    /// The response carried only `errors`.
    #[error("{}", .errors.first().map(|e| e.message.as_str()).unwrap_or("GraphQL request failed"))]
    GraphQl { errors: Vec<GraphQlError> },

    /// The instance does not know fields we asked for — typically premium fields on a
    /// Free-tier instance.
    ///
    /// Plural because one response names every offending field at once, and retrying
    /// them one at a time would cost a round trip per field.
    #[error("this GitLab instance does not support: {}", .fields.join(", "))]
    UnknownField { fields: Vec<String> },

    /// The document was rejected for exceeding the analyzed complexity or depth limit.
    #[error("query exceeded this instance's GraphQL {} limit{}", .kind.as_str(), match .limit {
        Some(n) => format!(" of {n}"),
        None => String::new(),
    })]
    LimitExceeded { kind: LimitKind, limit: Option<u32> },

    /// A failure with no more specific classification.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// How the application should respond, given where in its lifetime it is.
    pub const fn recovery(&self, phase: Phase) -> Recovery {
        match self {
            // Nothing to show and no way to proceed.
            Self::Config(_) => Recovery::Fatal,

            // Fatal at startup, but at runtime the last snapshot is still
            // worth looking at, so stop refreshing rather than tearing the TUI down.
            Self::Unauthorized { .. } | Self::NoCurrentUser { .. } => match phase {
                Phase::Startup => Recovery::Fatal,
                Phase::Runtime => Recovery::PauseRefreshes,
            },

            // The startup probe has no snapshot to fall back on, so a transport failure
            // there is fatal; afterwards it is just a refresh that did not land.
            Self::RateLimited { retry_after } => match phase {
                Phase::Startup => Recovery::Fatal,
                Phase::Runtime => Recovery::Backoff {
                    retry_after: *retry_after,
                },
            },
            Self::Http { .. } | Self::Network(_) | Self::GraphQl { .. } | Self::Other(_) => {
                match phase {
                    Phase::Startup => Recovery::Fatal,
                    Phase::Runtime => Recovery::Backoff { retry_after: None },
                }
            }

            Self::GraphQlPartial { .. } => Recovery::RenderPartial,
            Self::UnknownField { .. } => Recovery::ReduceFragment,
            Self::LimitExceeded { .. } => Recovery::DegradeComplexity,
        }
    }

    /// The process exit code for this error: 2 for configuration and authentication
    /// failures, 1 for everything else.
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) | Self::Unauthorized { .. } | Self::NoCurrentUser { .. } => EXIT_CONFIG,
            Self::RateLimited { .. }
            | Self::Http { .. }
            | Self::Network(_)
            | Self::GraphQlPartial { .. }
            | Self::GraphQl { .. }
            | Self::UnknownField { .. }
            | Self::LimitExceeded { .. }
            | Self::Other(_) => EXIT_FAILURE,
        }
    }

    /// The exit code as a [`ExitCode`], for returning from `main`.
    pub fn exit_status(&self) -> ExitCode {
        ExitCode::from(self.exit_code())
    }

    /// The server's retry instruction, when it sent one.
    #[cfg(test)]
    pub(crate) const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::Config(_)
            | Self::Unauthorized { .. }
            | Self::NoCurrentUser { .. }
            | Self::Http { .. }
            | Self::Network(_)
            | Self::GraphQlPartial { .. }
            | Self::GraphQl { .. }
            | Self::UnknownField { .. }
            | Self::LimitExceeded { .. }
            | Self::Other(_) => None,
        }
    }
}

/// Runtime failure.
pub const EXIT_FAILURE: u8 = 1;
/// Configuration or authentication failure.
pub const EXIT_CONFIG: u8 = 2;

/// A `Result` carrying the crate's error taxonomy.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Install `color-eyre` for top-level error and panic reporting.
///
/// This registers a panic hook. The terminal guard wraps that hook so
/// restoration always runs *before* a report is printed — a backtrace rendered onto the
/// alternate screen in raw mode is unreadable and is lost the moment the screen is
/// restored. Call this once, from `main`, before the guard is installed.
pub fn install_reporting() -> color_eyre::Result<()> {
    color_eyre::install()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_err() -> Error {
        Error::Config(ConfigError::MissingToken)
    }

    fn unauthorized(status: u16) -> Error {
        Error::Unauthorized {
            status,
            instance: "https://gitlab.example.com".into(),
            token_source: TokenSource::GitlabTokenEnv,
        }
    }

    #[test]
    fn exit_codes_match_spec() {
        // Clean exit is `ExitCode::SUCCESS`, used directly in `main` rather than through a
        // constant here.
        assert_eq!(EXIT_FAILURE, 1);
        assert_eq!(EXIT_CONFIG, 2);

        assert_eq!(config_err().exit_code(), EXIT_CONFIG, "config failures");
        assert_eq!(
            Error::Config(ConfigError::Invalid {
                key: "refresh.interval_secs".into(),
                message: "must be at least 30".into(),
            })
            .exit_code(),
            EXIT_CONFIG,
        );
        assert_eq!(
            unauthorized(401).exit_code(),
            EXIT_CONFIG,
            "auth failures exit 2, not 1",
        );
        assert_eq!(Error::Http { status: 503 }.exit_code(), EXIT_FAILURE);
        assert_eq!(
            Error::Network("connection refused".into()).exit_code(),
            EXIT_FAILURE,
        );
        assert_eq!(
            Error::LimitExceeded {
                kind: LimitKind::Depth,
                limit: Some(15)
            }
            .exit_code(),
            EXIT_FAILURE,
        );
    }

    #[test]
    fn auth_failure_is_fatal_at_startup_but_pauses_at_runtime() {
        let err = unauthorized(403);
        assert_eq!(err.recovery(Phase::Startup), Recovery::Fatal);
        assert_eq!(err.recovery(Phase::Runtime), Recovery::PauseRefreshes);
    }

    #[test]
    fn config_failure_is_fatal_in_both_phases() {
        assert_eq!(config_err().recovery(Phase::Startup), Recovery::Fatal);
        assert_eq!(config_err().recovery(Phase::Runtime), Recovery::Fatal);
    }

    #[test]
    fn transport_failures_back_off_at_runtime() {
        let after = Some(Duration::from_secs(30));
        assert_eq!(
            Error::RateLimited { retry_after: after }.recovery(Phase::Runtime),
            Recovery::Backoff { retry_after: after },
            "a Retry-After header overrides our own backoff"
        );
        assert_eq!(
            Error::Http { status: 502 }.recovery(Phase::Runtime),
            Recovery::Backoff { retry_after: None },
        );
    }

    #[test]
    fn degradable_failures_keep_the_table_populated() {
        let partial = Error::GraphQlPartial {
            errors: vec![GraphQlError::new("boom")],
        };
        assert_eq!(partial.recovery(Phase::Runtime), Recovery::RenderPartial);
        assert_eq!(
            Error::UnknownField {
                fields: vec!["approvalsLeft".into()]
            }
            .recovery(Phase::Runtime),
            Recovery::ReduceFragment,
        );
        assert_eq!(
            Error::LimitExceeded {
                kind: LimitKind::Complexity,
                limit: Some(250),
            }
            .recovery(Phase::Runtime),
            Recovery::DegradeComplexity,
        );
    }

    #[test]
    fn retry_after_is_only_carried_by_rate_limiting() {
        let d = Duration::from_secs(12);
        assert_eq!(
            Error::RateLimited {
                retry_after: Some(d)
            }
            .retry_after(),
            Some(d)
        );
        assert_eq!(Error::Http { status: 500 }.retry_after(), None);
    }

    #[test]
    fn missing_token_message_names_all_four_sources() {
        let msg = ConfigError::MissingToken.to_string();
        for source in ["MRQ_TOKEN", "GITLAB_TOKEN", "token_command", "token"] {
            assert!(msg.contains(source), "missing `{source}` in: {msg}");
        }
    }

    #[test]
    fn an_auth_failure_names_the_instance_and_the_token_source() {
        let msg = unauthorized(401).to_string();
        for expected in [
            "https://gitlab.example.com",
            "$GITLAB_TOKEN",
            "401",
            "read_api",
            "gitlab.url",
        ] {
            assert!(msg.contains(expected), "missing `{expected}` in: {msg}");
        }
    }

    #[test]
    fn limit_errors_name_the_limit_when_known() {
        let err = Error::LimitExceeded {
            kind: LimitKind::Complexity,
            limit: Some(250),
        };
        let msg = err.to_string();
        assert!(msg.contains("complexity"), "{msg}");
        assert!(msg.contains("250"), "{msg}");
    }
}
