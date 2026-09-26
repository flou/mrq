//! `mrq` — a keyboard-driven TUI for the GitLab merge requests you need to review.

// Tests use unwrap/expect constantly and legitimately; production code does not, and a
// documented exception gets its own #[expect] at the call site instead of a blanket allow.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod app;
mod cli;
mod config;
mod error;
mod gitlab;
mod logging;
mod term;
mod ui;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use crate::app::event::QuitReason;
use crate::cli::{Cli, Command};
use crate::config::keymap;
use crate::config::paths::{Env, Paths};
use crate::config::schema::Filter;
use crate::config::token::{self, Token, TokenEnv};
use crate::config::validate::Clamped;
use crate::config::{load, validate};
use crate::error::{ConfigError, Error, LimitKind};
use crate::gitlab::client::Client;
use crate::gitlab::{probe, query};
use clap_complete::Shell;

fn main() -> ExitCode {
    // Before the terminal guard installs its own hook (`term::guard::install_panic_hook`
    // wraps whatever hook is already there): color-eyre must be the "previous" hook it
    // wraps, or a panic prints its multi-line report into the alternate screen instead of
    // after it's restored. A failure here just means a plainer panic report.
    let _ = error::install_reporting();

    let cli = Cli::parse();

    // Logging is set up here, not inside `run`, so the appender outlives the error
    // report below. Held in `run` instead, its worker thread stops when `run` returns
    // and the fatal error — the one line most worth having — never reaches the file.
    let log = match setup_logging(&cli) {
        Ok(log) => log,
        Err(err) => {
            eprintln!("mrq: {err}");
            return err.exit_status();
        }
    };

    let result = run(&cli, &log);
    match result {
        Ok(code) => code,
        Err(err) => {
            // Logged *and* printed. Printed because a fatal error the user cannot see is
            // the same as no error at all, and by this point the guard has restored the
            // terminal. Logged because the log is what gets pasted into a bug report,
            // and a log that omits the failure is the one thing it must not do.
            tracing::error!(%err, "fatal");
            eprintln!("mrq: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            err.exit_status()
        }
    }
}

/// Resolve paths and install the subscriber.
///
/// Runs before the config is parsed so that parse failures are recorded, but after the
/// paths are known so the file lands in the state directory.
fn setup_logging(cli: &Cli) -> Result<logging::LogHandle, Error> {
    // Rejected before anything else happens: a bad --log-level would otherwise silently
    // fall back to `info` and look like logging is broken.
    let level = cli.log_level().map_err(|bad| {
        Error::Config(ConfigError::Invalid {
            key: "--log-level".into(),
            message: format!("`{bad}` is not a level; expected trace, debug, info, warn or error"),
        })
    })?;

    let paths = Paths::resolve(&Env::from_process(), cli.config.as_deref())?;
    let log_path = match &cli.log {
        Some(explicit) => Some(explicit.clone()),
        None => paths
            .ensure_state_dir()
            .ok()
            .map(crate::logging::default_log_path),
    };

    let log = logging::init(log_path.as_deref(), level);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "mrq starting");
    Ok(log)
}

fn run(cli: &Cli, log: &logging::LogHandle) -> Result<ExitCode, Error> {
    // Ahead of `load::load`: the schema is static and must stay usable to diagnose a
    // config file that does not parse, so it cannot depend on that file loading first.
    if cli.command == Some(Command::Schema) {
        return schema(&mut std::io::stdout().lock()).map(|()| ExitCode::SUCCESS);
    }

    let paths = Paths::resolve(&Env::from_process(), cli.config.as_deref())?;
    let mut loaded = load::load(paths, &cli.overrides())?;

    // The config layer promises a *validated* Config (see config/mod.rs); a filter that
    // relies on `validate` catching it — a root-only argument on a current-user scope, an
    // out-of-range max_results — must never reach the rest of the program unvalidated.
    // The clamps are kept rather than dropped: `check` has to report them, because it
    // prints the clamped value and nothing else would say it had been changed.
    let clamps = validate::validate(&mut loaded.config)?;
    for clamp in &clamps {
        tracing::warn!(%clamp, "config value out of range, clamped");
    }

    match &cli.command {
        Some(Command::InitConfig { force }) => {
            init_config(&loaded, *force, &mut std::io::stdout().lock()).map(|()| ExitCode::SUCCESS)
        }
        Some(Command::Check { offline }) => check(
            &loaded,
            &clamps,
            &TokenEnv::from_process(),
            *offline,
            &mut std::io::stdout().lock(),
        )
        .map(|()| ExitCode::SUCCESS),
        Some(Command::Schema) => unreachable!("handled above, before the config was loaded"),
        Some(Command::Completion { shell }) => {
            completion((*shell).into(), &mut std::io::stdout().lock()).map(|()| ExitCode::SUCCESS)
        }
        None => tui(loaded, log.buffer()),
    }
}

/// Write the commented default config file.
///
/// The destination is the *default* XDG path, not the resolved one: `--config` names a
/// file to read, and writing a template over whatever the user pointed at would be a
/// surprising reading of it.
///
/// Takes its sink as an argument rather than calling `println!` — nothing in this binary
/// writes to stdout unconditionally, because a stray write corrupts the alternate screen
/// once the TUI owns it.
fn init_config(
    loaded: &load::Loaded,
    force: bool,
    out: &mut impl std::io::Write,
) -> Result<(), Error> {
    let path = loaded.paths.default_config_file();
    let replaced = config::init::write_default(path, force)?;

    let verb = if replaced { "replaced" } else { "wrote" };
    writeln!(out, "{verb} {}", path.display()).map_err(|e| Error::Other(e.to_string()))?;
    Ok(())
}

/// Validate the config, verify the token, confirm connectivity, and report the
/// instance's real complexity ceiling — the supported way to debug auth and filter
/// configuration without entering the TUI (§9.1, §4.5).
///
/// Config and filters print first regardless of what happens next: they are the more
/// common reason someone reaches for this command, and they are worth showing even when
/// the network call after them fails.
///
/// `--offline` ends the command at the token line. Every stage up to there is local, so
/// the whole config half runs with no connection and no token — which is what a CI job or
/// a pre-commit hook has.
fn check(
    loaded: &load::Loaded,
    clamps: &[Clamped],
    token_env: &TokenEnv,
    offline: bool,
    out: &mut impl std::io::Write,
) -> Result<(), Error> {
    print_resolved_config(loaded, out)?;
    print_clamps(clamps, out)?;
    print_resolved_filters(&loaded.config.filters, out)?;

    // The last no-network stage of the config pipeline, and one `check` used to skip: a
    // key claimed by two actions is fatal in `app::run` and the user looks here to find
    // out why, so the check belongs here rather than at the first frame of the TUI.
    keymap::resolve(&loaded.config.keys)?;

    let resolved = token::resolve(&loaded.config.gitlab, token_env, loaded.paths.config_file());

    // A token is only needed to open the connection, and `--offline` opens none: report
    // the outcome and stop. A token that does not resolve is not a configuration error
    // for a command that never connects, which is what makes this callable from a CI job
    // or a pre-commit hook where no token exists.
    if offline {
        return match &resolved {
            Ok(resolved) => print_token(resolved, out),
            Err(err) => writeln!(out, "token: not resolved: {err}").map_err(io_err),
        };
    }

    let resolved = resolved?;
    print_token(&resolved, out)?;
    check_instance(loaded, resolved.token, out)
}

/// Report where the token came from, never its value.
fn print_token(resolved: &token::Resolved, out: &mut impl std::io::Write) -> Result<(), Error> {
    for warning in &resolved.warnings {
        writeln!(out, "token: warning: {warning}").map_err(io_err)?;
    }
    writeln!(out, "token: resolved from {}", resolved.token.source()).map_err(io_err)?;
    Ok(())
}

/// The half of `check` that needs a network: confirm the token authenticates, then report
/// the instance's real complexity ceiling.
fn check_instance(
    loaded: &load::Loaded,
    token: Token,
    out: &mut impl std::io::Write,
) -> Result<(), Error> {
    let client = Client::new(&loaded.config.gitlab, token)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Other(format!("could not start the async runtime: {e}")))?;

    let identity = runtime.block_on(probe::identify(&client))?;
    writeln!(
        out,
        "gitlab: connected to {} as {} (instance version {})",
        client.endpoint(),
        identity.username,
        identity.version.as_deref().unwrap_or("unknown"),
    )
    .map_err(io_err)?;

    match runtime.block_on(probe_complexity_limit(&client)) {
        Some(limit) => writeln!(out, "complexity: this instance's limit is {limit}"),
        None => writeln!(
            out,
            "complexity: could not be determined (the probe was not rejected the way a \
             complexity ceiling would reject it)"
        ),
    }
    .map_err(io_err)?;

    Ok(())
}

fn io_err(e: std::io::Error) -> Error {
    Error::Other(e.to_string())
}

/// Print the resolved value and source of every configuration key.
///
/// The token's own value is never printed, only where it came from (`token:`, further
/// down `check`, already covers that). `token_command` keeps only its program name: the
/// full line commonly carries a secret as an argument, the same reasoning
/// `ConfigError::TokenCommand` redacts it for.
fn print_resolved_config(
    loaded: &load::Loaded,
    out: &mut impl std::io::Write,
) -> Result<(), Error> {
    writeln!(out, "config:").map_err(io_err)?;
    for (key, source, value) in load::resolved_entries(&loaded.config, &loaded.provenance) {
        let value = match key.as_str() {
            "gitlab.token" => "<redacted>".to_owned(),
            "gitlab.token_command" => {
                let program = value.split_whitespace().next().unwrap_or(&value);
                format!("{program} <arguments hidden>")
            }
            _ => value,
        };
        writeln!(out, "  {key} = {value} ({})", source.as_str()).map_err(io_err)?;
    }
    Ok(())
}

/// Report the values `validate` changed in place, which the report above would otherwise
/// print as ordinary resolved values: `filter[0].max_results = 500 (file)` is
/// indistinguishable from a user who wrote 500.
fn print_clamps(clamps: &[Clamped], out: &mut impl std::io::Write) -> Result<(), Error> {
    for clamp in clamps {
        writeln!(out, "config: warning: {clamp}").map_err(io_err)?;
    }
    Ok(())
}

/// Print each configured filter's query root and the arguments it resolves to, using the
/// same builder the fetch path does — so what `check` prints is what would actually be
/// sent, not a paraphrase of it.
fn print_resolved_filters(filters: &[Filter], out: &mut impl std::io::Write) -> Result<(), Error> {
    writeln!(out, "filters:").map_err(io_err)?;
    let now = jiff::Timestamp::now();
    for filter in filters {
        let document = query::build(
            filter,
            &query::Fragment::full(),
            query::FALLBACK_PAGE_SIZES[0],
            None,
            now,
        );
        writeln!(
            out,
            "  {} -> {}\n    {}",
            filter.name,
            query::root_description(filter),
            document.variables
        )
        .map_err(io_err)?;
    }
    Ok(())
}

/// Send the deliberately over-budget probe and read the instance's complexity ceiling
/// out of the rejection (§4.5). `None` covers every way this fails to answer the
/// question — a transport failure, or a response that was not rejected for complexity —
/// because discovering the ceiling is a diagnostic bonus, never a reason to fail `mrq
/// check` itself.
async fn probe_complexity_limit(client: &Client) -> Option<u32> {
    let probe = query::over_budget_probe();
    let response = client
        .execute::<serde_json::Value, _>(&probe.query, &probe.variables)
        .await
        .ok()?;

    match response.failure() {
        Some(Error::LimitExceeded {
            kind: LimitKind::Complexity,
            limit,
        }) => limit,
        _ => None,
    }
}

/// Print the JSON Schema for `config.toml`, for editors to validate and autocomplete it.
fn schema(out: &mut impl std::io::Write) -> Result<(), Error> {
    let rendered = serde_json::to_string_pretty(&config::schema::json_schema())
        .map_err(|e| Error::Other(e.to_string()))?;
    writeln!(out, "{rendered}").map_err(|e| Error::Other(e.to_string()))?;
    Ok(())
}

/// Generate a shell completion script.
fn completion(shell: Shell, out: &mut impl std::io::Write) -> Result<(), Error> {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "mrq", out);
    Ok(())
}

/// Run the TUI.
///
/// The runtime is built here rather than with `#[tokio::main]` so that the
/// non-interactive subcommands, which need no runtime at all, do not pay for one.
fn tui(loaded: load::Loaded, log: logging::LogBuffer) -> Result<ExitCode, Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Other(format!("could not start the async runtime: {e}")))?;

    let reason = runtime.block_on(app::run::run(loaded, log))?;
    tracing::info!(?reason, "exited");
    Ok(match reason {
        QuitReason::User => ExitCode::SUCCESS,
        QuitReason::Signal(signal) => ExitCode::from(signal.exit_code()),
        QuitReason::TaskPanicked => ExitCode::from(crate::error::EXIT_FAILURE),
        // Unreachable in practice: `run` returns `Err` whenever it quits for this
        // reason, and the `?` above has already taken that branch. The arm exists
        // because `QuitReason` cannot say so at the type level.
        QuitReason::Fatal => ExitCode::from(crate::error::EXIT_FAILURE),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load::Overrides;
    use wiremock::MockServer;

    /// The smallest config that validates: one currentUser-scoped filter, no token.
    const MINIMAL: &str = r#"
[gitlab]
url = "https://gitlab.example.com"

[[filter]]
name = "Assigned"
scope = "assigned"
"#;

    const FILE_TOKEN: &str = r#"
[gitlab]
token = "glpat-from-the-file"

[[filter]]
name = "Assigned"
scope = "assigned"
"#;

    struct Report {
        _tmp: tempfile::TempDir,
        outcome: Result<(), Error>,
        printed: String,
    }

    /// The config half of `run` followed by `check`, in the order `run` uses, so a
    /// rejected config fails before a single line is printed.
    ///
    /// The environment is injected rather than read from the process for the reason
    /// `config::suite` gives: the harness runs tests as threads, and `check` resolving a
    /// token out of the developer's own `$MRQ_TOKEN` would make the token assertions
    /// depend on the machine.
    fn report(body: &str, token_env: &TokenEnv, offline: bool) -> Report {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, body).unwrap();

        let home = Env {
            home: Some(tmp.path().to_path_buf()),
            ..Env::default()
        };
        let paths = Paths::resolve(&home, Some(&path)).unwrap();
        let mut out = Vec::new();

        let outcome = load::load(paths, &Overrides::default())
            .map_err(Error::from)
            .and_then(|mut loaded| {
                let clamps = validate::validate(&mut loaded.config).map_err(Error::from)?;
                check(&loaded, &clamps, token_env, offline, &mut out)
            });

        Report {
            _tmp: tmp,
            outcome,
            printed: String::from_utf8(out).unwrap(),
        }
    }

    fn env_token(value: &str) -> TokenEnv {
        TokenEnv {
            mrq_token: Some(value.to_owned()),
            gitlab_token: None,
        }
    }

    /// The whole config half, and nothing that needs a connection.
    #[test]
    fn offline_reports_the_config_the_filters_and_the_token_source() {
        let r = report(MINIMAL, &env_token("glpat-x"), true);
        r.outcome.unwrap();

        assert!(r.printed.contains("gitlab.url ="), "{}", r.printed);
        assert!(r.printed.contains("Assigned -> "), "{}", r.printed);
        assert!(
            r.printed.contains("token: resolved from $MRQ_TOKEN"),
            "{}",
            r.printed
        );
        assert!(!r.printed.contains("gitlab: connected"), "{}", r.printed);
        assert!(!r.printed.contains("complexity:"), "{}", r.printed);
        assert!(
            !r.printed.contains("glpat-x"),
            "the token value must not be printed: {}",
            r.printed
        );
    }

    /// The acceptance criterion, proved rather than asserted: the instance is a live
    /// listener, so a request that slipped through would be recorded.
    #[tokio::test]
    async fn offline_opens_no_connection() {
        let server = MockServer::start().await;
        let body = format!(
            "[gitlab]\nurl = \"{}\"\n\n[[filter]]\nname = \"Assigned\"\nscope = \"assigned\"\n",
            server.uri()
        );

        report(&body, &env_token("glpat-x"), true).outcome.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.is_empty(),
            "the offline path sent {} request(s)",
            requests.len()
        );
    }

    /// A CI job or a pre-commit hook has no token, and validating a config does not need
    /// one: the run succeeds and says where it looked.
    #[test]
    fn offline_succeeds_with_no_token_anywhere() {
        let r = report(MINIMAL, &TokenEnv::default(), true);
        r.outcome.unwrap();

        assert!(r.printed.contains("token: not resolved"), "{}", r.printed);
    }

    #[test]
    fn offline_reports_a_token_from_the_config_file() {
        let r = report(FILE_TOKEN, &TokenEnv::default(), true);
        r.outcome.unwrap();

        assert!(
            r.printed.contains("token: resolved from the config file"),
            "{}",
            r.printed
        );
        assert!(
            !r.printed.contains("glpat-from-the-file"),
            "the token value must not be printed: {}",
            r.printed
        );
    }

    /// A validator that cannot fail is a report, not a check.
    #[test]
    fn offline_fails_on_a_config_error() {
        let r = report("[refresh]\ninterval_secs = 5\n", &TokenEnv::default(), true);
        let err = r.outcome.unwrap_err();

        assert_eq!(err.exit_code(), crate::error::EXIT_CONFIG, "{err}");
        assert!(err.to_string().contains("interval_secs"), "{err}");
    }

    /// A value `validate` changed in place is otherwise printed as if the user had
    /// written it.
    #[test]
    fn a_clamped_value_is_reported_rather_than_shown_silently_changed() {
        let r = report(
            "[[filter]]\nname = \"Assigned\"\nscope = \"assigned\"\nmax_results = 5000\n",
            &TokenEnv::default(),
            true,
        );
        r.outcome.unwrap();

        assert!(
            r.printed
                .contains("filter[0].max_results: 5000 is out of range, using 500"),
            "{}",
            r.printed
        );
    }

    /// A key claimed by two actions is fatal when the TUI starts, and `check` is where
    /// someone looks for the reason — so it fails here too, in both modes. It is checked
    /// before the token on purpose: `check` used to pass a duplicate keybind and fail only
    /// at the first frame.
    #[test]
    fn a_duplicate_keybind_fails_the_check_in_both_modes() {
        for offline in [false, true] {
            let r = report(
                "[keys]\ntoggle_drafts = [\"o\"]\n",
                &TokenEnv::default(),
                offline,
            );
            let err = r.outcome.unwrap_err();

            assert!(
                matches!(err, Error::Config(ConfigError::DuplicateKeybind { .. })),
                "offline={offline}: {err:?}"
            );
        }
    }

    /// The no-flag path is unchanged: the connection needs a token, so a missing one is
    /// still fatal — and the config report is still worth printing first.
    #[test]
    fn a_missing_token_is_still_fatal_without_the_flag() {
        let r = report(MINIMAL, &TokenEnv::default(), false);
        let err = r.outcome.unwrap_err();

        assert!(
            matches!(err, Error::Config(ConfigError::MissingToken)),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), crate::error::EXIT_CONFIG, "{err}");
        assert!(r.printed.contains("config:"), "{}", r.printed);
        assert!(r.printed.contains("filters:"), "{}", r.printed);
    }
}
