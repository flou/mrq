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

use std::io::Write;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use crate::app::event::QuitReason;
use crate::cli::{Cli, Command};
use crate::config::paths::{Env, Paths};
use crate::config::schema::Filter;
use crate::config::token::{self, TokenEnv};
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
    for clamp in validate::validate(&mut loaded.config)? {
        tracing::warn!(%clamp, "config value out of range, clamped");
    }

    match &cli.command {
        Some(Command::InitConfig { force }) => {
            init_config(&loaded, *force, &mut std::io::stdout().lock()).map(|()| ExitCode::SUCCESS)
        }
        Some(Command::Check) => check(&loaded).map(|()| ExitCode::SUCCESS),
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
fn check(loaded: &load::Loaded) -> Result<(), Error> {
    let mut out = std::io::stdout().lock();

    print_resolved_config(loaded, &mut out)?;
    print_resolved_filters(&loaded.config.filters, &mut out)?;

    let resolved = token::resolve(
        &loaded.config.gitlab,
        &TokenEnv::from_process(),
        loaded.paths.config_file(),
    )?;
    for warning in &resolved.warnings {
        writeln!(out, "token: warning: {warning}").map_err(io_err)?;
    }
    writeln!(out, "token: resolved from {}", resolved.token.source()).map_err(io_err)?;

    let client = Client::new(&loaded.config.gitlab, resolved.token)?;
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
