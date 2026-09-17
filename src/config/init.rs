//! `mrq init-config`: writing the commented default configuration file.
//!
//! New users need a starting point that documents itself, so the shipped
//! file lists every key with its built-in default — commented out, which is what makes
//! the round-trip property below hold.
//!
//! # The round-trip property
//!
//! Parsing [`DEFAULT_CONFIG`] yields exactly [`Config::default()`]. That is not a
//! coincidence to be preserved by hand: [`the_shipped_default_is_the_built_in_default`]
//! asserts it, so a comment that documents a default the code no longer has is a test
//! failure rather than a lie that ships. It is also why the file is written as comments —
//! an uncommented value would be indistinguishable from a user's choice, and every
//! default change would need the file edited in lockstep.

use std::io::Write;
use std::path::Path;

#[cfg(test)]
use crate::config::schema::Config;
use crate::error::ConfigError;

/// The commented default configuration file, exactly as `init-config` writes it.
pub const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

/// The mode the file is created with.
///
/// A literal `token` is allowed in the config file, and a user who adds one should not
/// have to notice that the file mrq generated was world-readable. Owner-only from the
/// start costs nothing when there is no token in it.
const CONFIG_MODE: u32 = 0o600;

/// Write [`DEFAULT_CONFIG`] to `path`, creating parent directories as needed.
///
/// Refuses to overwrite an existing file unless `force`. Returns whether an existing file
/// was replaced, so the caller can say which of the two things happened.
pub fn write_default(path: &Path, force: bool) -> Result<bool, ConfigError> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let io = |source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|source| ConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
    }

    // Sampled before the open: `force` opens with `truncate`, so afterwards every file
    // looks new.
    let replaced = force && path.is_file();

    // `create_new` rather than a prior `exists()` check: the refusal is the open itself,
    // so there is no window between the check and the write in which a file could appear.
    let mut options = OpenOptions::new();
    options.write(true).mode(CONFIG_MODE);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }

    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ConfigError::Invalid {
                key: path.display().to_string(),
                message: "already exists; pass --force to overwrite it".into(),
            });
        }
        Err(e) => return Err(io(e)),
    };

    // `mode` on OpenOptions only applies to a file it creates, so an overwrite of a
    // permissive existing file would keep the permissive mode.
    if force {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(CONFIG_MODE))
            .map_err(&io)?;
    }

    file.write_all(DEFAULT_CONFIG.as_bytes()).map_err(&io)?;
    Ok(replaced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The property the whole design of the file rests on: the comments cannot drift from
    /// the code, because a default changed in one place and not the other fails here.
    #[test]
    fn the_shipped_default_is_the_built_in_default() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG)
            .unwrap_or_else(|e| panic!("the shipped default config must parse: {e}"));
        assert_eq!(parsed, Config::default());
    }

    /// Every commented key must be a key the schema still has. A stale `# jitter = 15`
    /// documents a key that would be rejected if the user uncommented it, which is worse
    /// than not documenting it at all.
    #[test]
    fn uncommenting_any_line_still_parses() {
        let mut checked = 0;
        for (n, line) in DEFAULT_CONFIG.lines().enumerate() {
            let Some(body) = line.strip_prefix("# ").map(str::trim_end) else {
                continue;
            };
            let Some(setting) = as_setting(body) else {
                continue;
            };
            checked += 1;
            let doc = format!("{}\n{setting}\n", enclosing_table(DEFAULT_CONFIG, n));
            assert!(
                toml::from_str::<Config>(&doc).is_ok(),
                "line {}: uncommenting `{setting}` is rejected by the schema",
                n + 1
            );
        }
        assert!(checked > 30, "only checked {checked} lines; parser broken?");
    }

    /// A commented line that is a `key = value` setting rather than prose, with any
    /// trailing `# comment` stripped.
    ///
    /// Prose in this file routinely contains ` = ` — "used when backend = \"command\"" —
    /// so the discriminator is the *left* side: a bare TOML key and nothing else.
    fn as_setting(body: &str) -> Option<&str> {
        let (key, _) = body.split_once(" = ")?;
        let bare = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        bare.then(|| body.split(" #").next().unwrap_or(body).trim_end())
    }

    /// The nearest preceding uncommented table header, so a commented setting can be
    /// parsed in the section it actually belongs to.
    fn enclosing_table(text: &str, line: usize) -> &str {
        text.lines()
            .take(line)
            .filter(|l| l.starts_with('['))
            .last()
            .unwrap_or("")
    }

    /// Documenting a binding that the grammar rejects would be worse than not documenting
    /// it; and the specs shown must be the ones actually shipped.
    #[test]
    fn the_documented_keybinds_match_the_shipped_defaults() {
        use crate::config::keymap::{DEFAULT_BINDINGS, parse_key};

        let keys: std::collections::BTreeMap<String, Vec<String>> = DEFAULT_CONFIG
            .lines()
            .skip_while(|l| !l.starts_with("[keys]"))
            .filter_map(|l| l.strip_prefix("# "))
            .filter_map(|body| body.split_once(" = "))
            .map(|(action, specs)| {
                let specs: Vec<String> = specs
                    .split('#')
                    .next()
                    .unwrap_or(specs)
                    .trim()
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
                (action.trim().to_owned(), specs)
            })
            .collect();

        assert_eq!(
            keys.len(),
            DEFAULT_BINDINGS.len(),
            "every action must be documented: {:?}",
            keys.keys().collect::<Vec<_>>()
        );

        for (action, specs) in DEFAULT_BINDINGS {
            let documented = keys
                .get(action.key())
                .unwrap_or_else(|| panic!("`{}` is not in the [keys] block", action.key()));

            let parsed = |s: &str| parse_key(s).unwrap_or_else(|e| panic!("`{s}`: {e}"));
            let want: Vec<_> = specs.iter().map(|s| parsed(s)).collect();
            let got: Vec<_> = documented.iter().map(|s| parsed(s)).collect();
            assert_eq!(got, want, "`{}` documents the wrong keys", action.key());
        }

        // And the table as written resolves, which is the other half: a documented spec
        // that parses but collides with another would still be a startup error.
        assert!(crate::config::keymap::resolve(&keys).is_ok());
    }

    #[test]
    fn writing_creates_an_owner_only_file_and_its_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/mrq/config.toml");

        let replaced = write_default(&path, false).unwrap();
        assert!(!replaced, "nothing was there to replace");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
        assert_eq!(mode_of(&path), CONFIG_MODE);
        assert_eq!(mode_of(path.parent().unwrap()), 0o700);
    }

    /// Refusing is the default, and the message has to say how to proceed.
    #[test]
    fn an_existing_file_is_not_overwritten_and_the_error_says_how() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[ui]\nmouse = true\n").unwrap();

        let err = write_default(&path, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already exists"), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
        assert!(msg.contains("config.toml"), "should name the file: {msg}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ui]\nmouse = true\n",
            "the user's file must be untouched"
        );
    }

    #[test]
    fn force_replaces_the_file_and_repairs_its_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[ui]\nmouse = true\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let replaced = write_default(&path, true).unwrap();
        assert!(replaced, "the caller needs to know a file was replaced");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
        assert_eq!(
            mode_of(&path),
            CONFIG_MODE,
            "an overwrite must not inherit the old permissive mode"
        );
    }

    /// A truncating write that left the tail of a longer previous file behind would
    /// produce a config that parses but is not the default.
    #[test]
    fn force_truncates_rather_than_overwriting_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "x".repeat(DEFAULT_CONFIG.len() * 2)).unwrap();

        write_default(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
    }

    #[test]
    fn force_on_a_missing_file_creates_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        let replaced = write_default(&path, true).unwrap();
        assert!(!replaced, "nothing existed, so nothing was replaced");
        assert_eq!(mode_of(&path), CONFIG_MODE);
    }

    /// An unwritable location must be reported as itself, not as a refusal to overwrite.
    #[test]
    fn an_unwritable_parent_is_an_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500)).unwrap();

        let err = write_default(&blocked.join("config.toml"), false).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }), "{err:?}");

        // Leave it writable or the TempDir cleanup fails.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// The file mrq writes must be one mrq itself accepts through the normal load path,
    /// not merely one `toml::from_str` tolerates.
    #[test]
    fn the_written_file_loads_and_validates() {
        use crate::config::load::{Overrides, load};
        use crate::config::paths::{Env, Paths};

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let path = home.join(".config/mrq/config.toml");
        write_default(&path, false).unwrap();

        let env = Env {
            home: Some(home),
            ..Env::default()
        };
        let mut loaded = load(Paths::resolve(&env, None).unwrap(), &Overrides::default()).unwrap();

        assert_eq!(loaded.config, Config::default());
        assert!(
            crate::config::validate::validate(&mut loaded.config)
                .unwrap()
                .is_empty(),
            "the shipped default must need no clamping"
        );
    }
}
