//! XDG resolution for the config file and the cache and state directories.
//!
//! MacOS uses the XDG layout here, *not* `~/Library/Application Support`:
//! `mrq` is a developer tool and `~/.config` is where its users keep this kind of file.
//! That rules out the platform-default strategies the usual directory crates provide, so
//! the layout is derived from `$HOME` and the `XDG_*` variables directly.
//!
//! Resolution reads its environment from an injected [`Env`] rather than calling
//! `std::env::var`. Process environment is global mutable state, and Rust runs tests in
//! threads of one process — tests that `set_var` race each other and pass or fail
//! depending on scheduling. Injecting the environment makes every case below a pure
//! function of its input.

use std::path::{Path, PathBuf};

use crate::error::ConfigError;

/// The environment variables that influence path resolution.
///
/// Construct with [`Env::from_process`] in the binary; construct literally in tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
    /// `$MRQ_CONFIG` — an explicit config file path.
    pub mrq_config: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |key| non_empty(std::env::var_os(key));
        Self {
            home: var("HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            xdg_cache_home: var("XDG_CACHE_HOME"),
            xdg_state_home: var("XDG_STATE_HOME"),
            mrq_config: var("MRQ_CONFIG"),
        }
    }

    fn home(&self) -> Result<&Path, ConfigError> {
        self.home.as_deref().ok_or(ConfigError::HomeNotFound)
    }

    /// An XDG base directory: the variable when set and absolute, otherwise the
    /// specified fallback under `$HOME`.
    ///
    /// The spec requires absolute paths for XDG variables and says relative ones must be
    /// ignored; honouring that keeps a stray `XDG_CACHE_HOME=cache` from scattering
    /// directories through whatever the working directory happened to be.
    fn base(&self, var: Option<&PathBuf>, fallback: &str) -> Result<PathBuf, ConfigError> {
        match var {
            Some(p) if p.is_absolute() => Ok(p.clone()),
            _ => Ok(self.home()?.join(fallback)),
        }
    }
}

/// Treat an empty environment value as unset.
///
/// `XDG_CONFIG_HOME=` in a shell profile is a common way to say "I did not set this", and
/// joining `mrq/config.toml` onto an empty path silently produces a *relative* path that
/// resolves against the working directory.
fn non_empty(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Where the config file was found, which decides how a missing file is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// `--config`. The user named this file, so it not existing is an error.
    Flag,
    /// `$MRQ_CONFIG`. Also explicitly named, also an error if absent.
    Env,
    /// A discovered XDG location. Absent simply means "run with defaults".
    Xdg,
}

impl ConfigSource {
    /// Whether a missing file at this location is a failure rather than a fallback.
    pub const fn is_explicit(self) -> bool {
        matches!(self, Self::Flag | Self::Env)
    }
}

/// The resolved filesystem layout for one `mrq` process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    config_file: Option<PathBuf>,
    config_source: ConfigSource,
    default_config_file: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
}

const APP: &str = "mrq";
const CONFIG_NAME: &str = "config.toml";

impl Paths {
    /// Resolve the layout from an environment and an optional `--config` flag.
    ///
    /// `--config` and `$MRQ_CONFIG` are returned even when the file is absent, so the
    /// caller can report *which* named file was missing rather than silently falling
    /// back to defaults and leaving the user wondering why their settings did nothing.
    pub fn resolve(env: &Env, flag: Option<&Path>) -> Result<Self, ConfigError> {
        let default_config_file = env
            .base(env.xdg_config_home.as_ref(), ".config")?
            .join(APP)
            .join(CONFIG_NAME);

        let (config_file, config_source) = if let Some(path) = flag {
            (Some(path.to_path_buf()), ConfigSource::Flag)
        } else if let Some(path) = &env.mrq_config {
            (Some(path.clone()), ConfigSource::Env)
        } else {
            // `$XDG_CONFIG_HOME/mrq/config.toml` and `~/.config/mrq/config.toml` are
            // separate candidates with "first hit wins", so when XDG_CONFIG_HOME points
            // somewhere without a config we still look in ~/.config rather than
            // reporting none. They collapse to one probe when XDG_CONFIG_HOME is unset
            // or already equal to ~/.config.
            let home_default = env.home()?.join(".config").join(APP).join(CONFIG_NAME);
            let found = [default_config_file.clone(), home_default]
                .into_iter()
                .find(|p| p.is_file());
            (found, ConfigSource::Xdg)
        };

        Ok(Self {
            config_file,
            config_source,
            default_config_file,
            cache_dir: env.base(env.xdg_cache_home.as_ref(), ".cache")?.join(APP),
            state_dir: env
                .base(env.xdg_state_home.as_ref(), ".local/state")?
                .join(APP),
        })
    }

    /// The config file to read, if there is one. `None` means "run with built-in
    /// defaults".
    pub fn config_file(&self) -> Option<&Path> {
        self.config_file.as_deref()
    }

    pub const fn config_source(&self) -> ConfigSource {
        self.config_source
    }

    /// Where `mrq init-config` should write.
    pub fn default_config_file(&self) -> &Path {
        &self.default_config_file
    }

    /// `$XDG_CACHE_HOME/mrq/` — filter snapshots for the warm first paint.
    #[cfg(test)]
    fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// `$XDG_STATE_HOME/mrq/` — the log file and notification dedup keys.
    #[cfg(test)]
    fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Create the cache directory, returning it.
    pub fn ensure_cache_dir(&self) -> Result<&Path, ConfigError> {
        ensure_private_dir(&self.cache_dir)?;
        Ok(&self.cache_dir)
    }

    /// Create the state directory, returning it.
    pub fn ensure_state_dir(&self) -> Result<&Path, ConfigError> {
        ensure_private_dir(&self.state_dir)?;
        Ok(&self.state_dir)
    }
}

/// The mode the cache and state directories are held at.
///
/// They hold merge-request titles and branch names from private projects, so they are
/// owner-only rather than whatever the user's umask happens to be.
const PRIVATE_MODE: u32 = 0o700;

/// Create a directory and its parents owner-only, or bring an existing one back to
/// [`PRIVATE_MODE`].
///
/// The repair matters as much as the creation: a directory left behind by another tool, a
/// pre-release of `mrq`, or a run under a looser umask would otherwise keep its mode
/// forever, and the privacy of the contents would be a property of whichever run happened
/// to create the directory rather than of `mrq`.
fn ensure_private_dir(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let io = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };

    if path.is_dir() {
        // Conditional so the common case is a stat and nothing else, and so a directory
        // that is already correct does not fail on a filesystem that refuses chmod.
        let mode = std::fs::metadata(path).map_err(&io)?.permissions().mode() & 0o777;
        if mode != PRIVATE_MODE {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_MODE))
                .map_err(&io)?;
        }
        return Ok(());
    }

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_MODE)
        .create(path)
        .map_err(&io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &Path) -> Env {
        Env {
            home: Some(home.to_path_buf()),
            ..Env::default()
        }
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    /// The whole reason this module hand-rolls XDG instead of using a platform-default
    /// strategy. On macOS the platform default is ~/Library/Application Support, which is
    /// not where anyone looks for a developer tool's dotfile.
    #[test]
    fn macos_uses_xdg_not_library() {
        let home = PathBuf::from("/Users/someone");
        let paths = Paths::resolve(&env(&home), None).unwrap();

        assert_eq!(
            paths.default_config_file(),
            home.join(".config/mrq/config.toml")
        );
        assert_eq!(paths.cache_dir(), home.join(".cache/mrq"));
        assert_eq!(paths.state_dir(), home.join(".local/state/mrq"));

        for p in [
            paths.default_config_file(),
            paths.cache_dir(),
            paths.state_dir(),
        ] {
            assert!(
                !p.to_string_lossy().contains("Library"),
                "{} must not resolve under ~/Library",
                p.display()
            );
        }
    }

    #[test]
    fn xdg_variables_override_the_home_defaults() {
        let e = Env {
            home: Some("/home/u".into()),
            xdg_config_home: Some("/cfg".into()),
            xdg_cache_home: Some("/cache".into()),
            xdg_state_home: Some("/state".into()),
            mrq_config: None,
        };
        let paths = Paths::resolve(&e, None).unwrap();

        assert_eq!(
            paths.default_config_file(),
            Path::new("/cfg/mrq/config.toml")
        );
        assert_eq!(paths.cache_dir(), Path::new("/cache/mrq"));
        assert_eq!(paths.state_dir(), Path::new("/state/mrq"));
    }

    /// A relative XDG value is ignored per the XDG spec. Honouring one would scatter
    /// directories through whatever the working directory happened to be.
    #[test]
    fn relative_xdg_values_fall_back_to_home() {
        let e = Env {
            home: Some("/home/u".into()),
            xdg_cache_home: Some("relative/cache".into()),
            ..Env::default()
        };
        let paths = Paths::resolve(&e, None).unwrap();
        assert_eq!(paths.cache_dir(), Path::new("/home/u/.cache/mrq"));
    }

    /// `XDG_CONFIG_HOME=` must behave as unset. Taken literally it would join onto "",
    /// producing a relative path resolved against the working directory.
    #[test]
    fn empty_env_values_are_treated_as_unset() {
        assert_eq!(non_empty(Some("".into())), None);
        assert_eq!(non_empty(None), None);
        assert_eq!(
            non_empty(Some("/cfg".into())),
            Some(PathBuf::from("/cfg")),
            "a real value still passes through"
        );

        // And the downstream effect: the HOME default applies, not a relative path.
        let e = Env {
            home: Some("/home/u".into()),
            xdg_config_home: non_empty(Some("".into())),
            ..Env::default()
        };
        let got = Paths::resolve(&e, None).unwrap();
        assert_eq!(
            got.default_config_file(),
            Path::new("/home/u/.config/mrq/config.toml")
        );
        assert!(got.default_config_file().is_absolute());
    }

    #[test]
    fn flag_wins_over_every_other_source() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let xdg = tmp.path().join("xdg");
        touch(&xdg.join("mrq/config.toml"));
        touch(&home.join(".config/mrq/config.toml"));

        let e = Env {
            home: Some(home),
            xdg_config_home: Some(xdg),
            mrq_config: Some(tmp.path().join("from-env.toml")),
            ..Env::default()
        };
        let flag = tmp.path().join("from-flag.toml");
        let paths = Paths::resolve(&e, Some(&flag)).unwrap();

        assert_eq!(paths.config_file(), Some(flag.as_path()));
        assert_eq!(paths.config_source(), ConfigSource::Flag);
    }

    #[test]
    fn mrq_config_wins_over_xdg() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        touch(&home.join(".config/mrq/config.toml"));

        let e = Env {
            home: Some(home),
            mrq_config: Some(tmp.path().join("from-env.toml")),
            ..Env::default()
        };
        let paths = Paths::resolve(&e, None).unwrap();

        assert_eq!(
            paths.config_file(),
            Some(tmp.path().join("from-env.toml").as_path())
        );
        assert_eq!(paths.config_source(), ConfigSource::Env);
    }

    /// An explicitly named file is reported even when it does not exist, so the caller
    /// can say which file was missing instead of silently using defaults.
    #[test]
    fn explicit_sources_are_reported_even_when_absent() {
        let e = Env {
            home: Some("/home/u".into()),
            mrq_config: Some("/nope/config.toml".into()),
            ..Env::default()
        };
        let paths = Paths::resolve(&e, None).unwrap();

        assert_eq!(paths.config_file(), Some(Path::new("/nope/config.toml")));
        assert!(paths.config_source().is_explicit());
        assert!(!ConfigSource::Xdg.is_explicit());
    }

    #[test]
    fn xdg_config_home_is_probed_before_home_default() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let xdg = tmp.path().join("xdg");
        touch(&xdg.join("mrq/config.toml"));
        touch(&home.join(".config/mrq/config.toml"));

        let e = Env {
            home: Some(home),
            xdg_config_home: Some(xdg.clone()),
            ..Env::default()
        };
        let paths = Paths::resolve(&e, None).unwrap();
        assert_eq!(
            paths.config_file(),
            Some(xdg.join("mrq/config.toml").as_path())
        );
    }

    /// The two XDG locations are separate ordered candidates, so a set XDG_CONFIG_HOME
    /// with nothing in it still falls through to ~/.config.
    #[test]
    fn home_default_is_probed_when_xdg_config_home_has_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let xdg = tmp.path().join("empty-xdg");
        touch(&home.join(".config/mrq/config.toml"));

        let e = Env {
            home: Some(home.clone()),
            xdg_config_home: Some(xdg),
            ..Env::default()
        };
        let paths = Paths::resolve(&e, None).unwrap();
        assert_eq!(
            paths.config_file(),
            Some(home.join(".config/mrq/config.toml").as_path())
        );
    }

    /// No config file anywhere is not an error — mrq runs on built-in defaults.
    #[test]
    fn missing_config_everywhere_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let e = env(&tmp.path().join("empty-home"));
        let paths = Paths::resolve(&e, None).unwrap();

        assert_eq!(paths.config_file(), None);
        assert_eq!(paths.config_source(), ConfigSource::Xdg);
        // ...but init-config still knows where it would go.
        assert!(paths.default_config_file().ends_with("mrq/config.toml"));
    }

    #[test]
    fn missing_home_is_an_error_rather_than_a_relative_path() {
        let err = Paths::resolve(&Env::default(), None).unwrap_err();
        assert!(matches!(err, ConfigError::HomeNotFound), "{err:?}");
    }

    /// A missing HOME still resolves when every directory is given explicitly, which is
    /// the case in minimal container environments.
    #[test]
    fn explicit_xdg_dirs_work_without_home() {
        let e = Env {
            home: None,
            xdg_config_home: Some("/cfg".into()),
            xdg_cache_home: Some("/cache".into()),
            xdg_state_home: Some("/state".into()),
            mrq_config: Some("/cfg/mrq/config.toml".into()),
        };
        let paths = Paths::resolve(&e, None).unwrap();
        assert_eq!(paths.cache_dir(), Path::new("/cache/mrq"));
        assert_eq!(paths.state_dir(), Path::new("/state/mrq"));
    }

    #[test]
    fn directories_are_created_on_demand_and_private() {
        let tmp = tempfile::tempdir().unwrap();
        let e = Env {
            home: Some(tmp.path().join("home")),
            ..Env::default()
        };
        let paths = Paths::resolve(&e, None).unwrap();

        assert!(!paths.cache_dir().exists());
        let cache = paths.ensure_cache_dir().unwrap();
        let state = paths.ensure_state_dir().unwrap();

        for dir in [cache, state] {
            assert!(dir.is_dir(), "{} was not created", dir.display());
            assert_eq!(
                mode_of(dir),
                PRIVATE_MODE,
                "{} should be owner-only",
                dir.display()
            );
        }

        // Creating an existing directory is a no-op, not an error.
        assert!(paths.ensure_cache_dir().is_ok());
    }

    /// A directory that already exists with a looser mode is repaired, not accepted. The
    /// contents are private by promise, and the promise has to hold on every run — not
    /// only on the run that happened to create the directory.
    #[test]
    fn an_existing_directory_with_loose_permissions_is_repaired() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let e = Env {
            home: Some(tmp.path().join("home")),
            ..Env::default()
        };
        let paths = Paths::resolve(&e, None).unwrap();

        for dir in [paths.cache_dir(), paths.state_dir()] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        paths.ensure_cache_dir().unwrap();
        paths.ensure_state_dir().unwrap();

        for dir in [paths.cache_dir(), paths.state_dir()] {
            assert_eq!(
                mode_of(dir),
                PRIVATE_MODE,
                "{} kept its group- and world-readable mode",
                dir.display()
            );
        }
    }

    /// Repairing must not disturb what is already inside — the cache is read on the same
    /// run that repairs the directory holding it.
    #[test]
    fn repairing_permissions_preserves_the_contents() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let e = Env {
            home: Some(tmp.path().join("home")),
            ..Env::default()
        };
        let paths = Paths::resolve(&e, None).unwrap();

        std::fs::create_dir_all(paths.cache_dir()).unwrap();
        let snapshot = paths.cache_dir().join("assigned-1a2b3c4d.json");
        std::fs::write(&snapshot, "[]").unwrap();
        std::fs::set_permissions(paths.cache_dir(), std::fs::Permissions::from_mode(0o777))
            .unwrap();

        paths.ensure_cache_dir().unwrap();

        assert_eq!(mode_of(paths.cache_dir()), PRIVATE_MODE);
        assert_eq!(std::fs::read_to_string(&snapshot).unwrap(), "[]");
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}
