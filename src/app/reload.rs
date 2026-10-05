//! Re-reading the configuration while the TUI runs.
//!
//! Split in two so a bad file can never leave the session half-reconfigured:
//! [`prepare`] does every fallible step — find the file, parse, validate, resolve the
//! keymap and skin, build a new client — and has no side effects, and only then does
//! `App` apply the result. An `Err` here means the running configuration was never
//! touched.

use std::path::PathBuf;

use crate::config::keymap::{self, Keymap};
use crate::config::load::{self, Overrides};
use crate::config::paths::{Env, Paths};
use crate::config::schema::Config;
use crate::config::token::{self, TokenEnv};
use crate::config::validate;
use crate::gitlab::client::Client;

/// What the command line contributed, so a reload resolves the file and applies the
/// overrides the same way startup did.
#[derive(Debug, Clone, Default)]
pub struct ReloadSource {
    /// `--config`.
    pub flag: Option<PathBuf>,
    /// `--refresh-secs` (and `--filter`, which a reload ignores).
    pub overrides: Overrides,
}

/// A configuration that passed every check and is ready to be applied.
pub struct Prepared {
    pub config: Config,
    pub keymap: Keymap,
    /// Only built when `[gitlab]` changed; otherwise the running client stays.
    pub client: Option<Client>,
}

/// Load and check the configuration without touching the running one.
///
/// Paths are resolved again rather than reused: an XDG config file created after launch
/// should be found.
pub fn prepare(
    env: &Env,
    token_env: &TokenEnv,
    source: &ReloadSource,
    current: &Config,
) -> Result<Prepared, String> {
    let paths = Paths::resolve(env, source.flag.as_deref()).map_err(|e| e.to_string())?;
    let mut loaded = load::load(paths, &source.overrides).map_err(|e| e.to_string())?;

    let clamps = validate::validate(&mut loaded.config).map_err(|e| e.to_string())?;
    for clamp in &clamps {
        tracing::warn!(%clamp, "config value out of range, clamped");
    }

    let keymap = keymap::resolve(&loaded.config.keys).map_err(|e| e.to_string())?;
    crate::ui::theme::check(&loaded.config.skin).map_err(|e| e.to_string())?;

    let client = if loaded.config.gitlab == current.gitlab {
        None
    } else {
        let resolved = token::resolve(&loaded.config.gitlab, token_env, loaded.paths.config_file())
            .map_err(|e| e.to_string())?;
        for warning in &resolved.warnings {
            tracing::warn!(%warning, "token");
        }
        Some(Client::new(&loaded.config.gitlab, resolved.token).map_err(|e| e.to_string())?)
    };

    Ok(Prepared {
        config: loaded.config,
        keymap,
        client,
    })
}

/// One line for the status bar. The full text goes to the log, reachable with `L`.
pub fn summarize(error: &str) -> String {
    let mut lines = error
        .lines()
        .map(|l| l.trim().trim_start_matches("- "))
        .filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("unknown error");
    match lines.count() {
        0 => format!("reload failed: {first}"),
        more => format!("reload failed: {first} (+{more} more, see log)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, body: &str) -> ReloadSource {
        let file = dir.join("config.toml");
        std::fs::write(&file, body).unwrap();
        ReloadSource {
            flag: Some(file),
            overrides: Overrides::default(),
        }
    }

    fn env(dir: &std::path::Path) -> Env {
        Env {
            home: Some(dir.to_path_buf()),
            ..Env::default()
        }
    }

    fn token_env() -> TokenEnv {
        TokenEnv {
            mrq_token: Some("glpat-test".into()),
            gitlab_token: None,
        }
    }

    fn prepared(body: &str) -> Result<Prepared, String> {
        let dir = tempfile::tempdir().unwrap();
        let source = write(dir.path(), body);
        prepare(&env(dir.path()), &token_env(), &source, &Config::default())
    }

    #[test]
    fn a_valid_file_is_prepared_with_its_keymap() {
        let ok = prepared("[keys]\nquit = [\"x\"]\n").ok().unwrap();
        assert_eq!(ok.keymap.keys_for(crate::config::keymap::Action::Quit).len(), 1);
        assert!(ok.client.is_none(), "gitlab is unchanged, so the client stays");
    }

    #[test]
    fn a_sub_minimum_interval_is_an_error() {
        let err = prepared("[refresh]\ninterval_secs = 5\n").err().unwrap();
        assert!(err.contains("interval"), "{err}");
    }

    #[test]
    fn a_duplicate_keybind_is_an_error() {
        let err = prepared("[keys]\nquit = [\"x\"]\nrefresh = [\"x\"]\n")
            .err()
            .unwrap();
        assert!(err.contains("bound to both"), "{err}");
    }

    #[test]
    fn an_unknown_skin_is_an_error() {
        assert!(prepared("[skin]\nname = \"no-such-skin\"\n").is_err());
    }

    #[test]
    fn a_syntax_error_is_an_error() {
        assert!(prepared("this is [not toml").is_err());
    }

    #[test]
    fn cli_overrides_apply_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = write(dir.path(), "");
        source.overrides.refresh_secs = Some(120);
        let ok = prepare(&env(dir.path()), &token_env(), &source, &Config::default())
            .ok()
            .unwrap();
        assert_eq!(ok.config.refresh.interval_secs, 120);
    }

    #[test]
    fn a_changed_gitlab_section_builds_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let source = write(dir.path(), "[gitlab]\nurl = \"https://gitlab.example.com\"\n");
        let ok = prepare(&env(dir.path()), &token_env(), &source, &Config::default())
            .ok()
            .unwrap();
        assert!(ok.client.is_some());
    }

    #[test]
    fn the_summary_keeps_one_line_and_counts_the_rest() {
        assert_eq!(
            summarize("config is invalid:\n  - a\n  - b"),
            "reload failed: config is invalid: (+2 more, see log)"
        );
        assert_eq!(summarize("boom"), "reload failed: boom");
    }
}
