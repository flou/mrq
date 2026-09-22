//! Reading the config file, layering CLI overrides, and recording where each value came
//! from.
//!
//! `mrq check` has to print the resolved source of every value — default, file, env
//! or flag — because the single most confusing configuration failure is a setting
//! that is being read from somewhere the user forgot about.
//!
//! Provenance is derived by walking the parsed TOML document for the keys it actually
//! contains, rather than by wrapping every field in a tracking type. Two reasons: the
//! schema stays plain serde structs that `init-config` can round-trip, and a key added to
//! the schema is tracked automatically instead of needing a matching tracker field.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::paths::Paths;
use crate::config::schema::Config;
use crate::error::ConfigError;

/// Where a configuration value came from.
///
/// `Env` is not constructed yet: no general config key can come from the environment
/// until a command's design says which ones may — only the token itself does, tracked
/// separately by [`super::token::TokenSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// The built-in default: the key was absent everywhere.
    Default,
    /// Present in the config file.
    File,
    /// An environment variable.
    #[cfg(test)]
    Env,
    /// A command-line flag.
    Flag,
}

impl Source {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::File => "file",
            #[cfg(test)]
            Self::Env => "env",
            Self::Flag => "flag",
        }
    }
}

/// The origin of each configured value, keyed by dotted path (`refresh.interval_secs`,
/// `filter[0].name`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance(BTreeMap<String, Source>);

impl Provenance {
    /// The source of a key. Anything not recorded came from the built-in defaults.
    pub(crate) fn get(&self, key: &str) -> Source {
        self.0.get(key).copied().unwrap_or(Source::Default)
    }

    fn set(&mut self, key: impl Into<String>, source: Source) {
        self.0.insert(key.into(), source);
    }

    /// Every non-default key, in path order, for `mrq check`.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, Source)> {
        self.0.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Record every key present in a parsed TOML document as coming from the file.
    fn record_document(&mut self, value: &toml::Value) {
        fn walk(prefix: &str, value: &toml::Value, out: &mut Provenance) {
            match value {
                toml::Value::Table(table) => {
                    for (key, child) in table {
                        let path = if prefix.is_empty() {
                            key.clone()
                        } else {
                            format!("{prefix}.{key}")
                        };
                        out.set(path.clone(), Source::File);
                        walk(&path, child, out);
                    }
                }
                toml::Value::Array(items) => {
                    for (i, child) in items.iter().enumerate() {
                        // Array-of-tables entries are addressable individually so that
                        // `mrq check` can attribute a specific filter's keys.
                        let path = format!("{prefix}[{i}]");
                        if child.is_table() {
                            out.set(path.clone(), Source::File);
                            walk(&path, child, out);
                        }
                    }
                }
                _ => {}
            }
        }
        walk("", value, self);
    }
}

/// Every leaf value in the resolved config, alongside its source, in path order.
///
/// Walks the *resolved* `Config` rather than a parsed file, so a key nobody set still
/// appears here with its built-in default. A field left at `None` (no `token` in the
/// file, no `path` on a currentUser-scoped filter) simply serializes to nothing and so
/// does not appear — there is no default value to report for it.
pub fn resolved_entries(config: &Config, provenance: &Provenance) -> Vec<(String, Source, String)> {
    #[expect(
        clippy::expect_used,
        reason = "every Config field is a plain scalar, string, Option, Vec or nested \
                  struct of the same, all of which toml::Value::try_from always accepts"
    )]
    let value = toml::Value::try_from(config).expect("Config always serializes to TOML");
    let mut out = Vec::new();
    collect_leaves(String::new(), &value, provenance, &mut out);
    out
}

fn collect_leaves(
    prefix: String,
    value: &toml::Value,
    provenance: &Provenance,
    out: &mut Vec<(String, Source, String)>,
) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_leaves(path, child, provenance, out);
            }
        }
        // An array of tables (`[[filter]]`) is addressed per entry, like `record_document`
        // does; any other array (`labels = [...]`, `columns = [...]`) is a leaf value.
        toml::Value::Array(items)
            if !items.is_empty() && items.iter().all(toml::Value::is_table) =>
        {
            for (i, child) in items.iter().enumerate() {
                collect_leaves(format!("{prefix}[{i}]"), child, provenance, out);
            }
        }
        toml::Value::Array(_)
        | toml::Value::String(_)
        | toml::Value::Integer(_)
        | toml::Value::Float(_)
        | toml::Value::Boolean(_)
        | toml::Value::Datetime(_) => {
            out.push((prefix.clone(), provenance.get(&prefix), render_leaf(value)));
        }
    }
}

/// Render a leaf TOML value for display. Never called on a `Table`: [`collect_leaves`]
/// always recurses into one instead of passing it here.
fn render_leaf(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(items) => format!(
            "[{}]",
            items.iter().map(render_leaf).collect::<Vec<_>>().join(", ")
        ),
        toml::Value::Table(_) => unreachable!("collect_leaves recurses into tables"),
    }
}

/// Command-line values that override the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `--refresh-secs`
    pub refresh_secs: Option<u64>,
    /// `--filter <name>`: which tab to open on, not a stored setting.
    pub filter: Option<String>,
}

/// A fully resolved configuration and the provenance of its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub config: Config,
    pub provenance: Provenance,
    pub paths: Paths,
    /// The filter named by `--filter`, to be resolved against the configured filters
    /// once they have been validated.
    pub initial_filter: Option<String>,
}

/// Read, parse and layer configuration.
///
/// Errors are returned rather than reported, because they must be printed before the
/// alternate screen is entered.
pub fn load(paths: Paths, overrides: &Overrides) -> Result<Loaded, ConfigError> {
    let mut provenance = Provenance::default();

    let mut config = match paths.config_file() {
        Some(path) => read_file(path, paths.config_source(), &mut provenance)?,
        None => Config::default(),
    };

    // The shipped `wide_columns` default names columns a user-written `columns` need not
    // contain. Only what they actually display can be wide-only.
    if provenance.get("ui.wide_columns") == Source::Default {
        config
            .ui
            .wide_columns
            .retain(|c| config.ui.columns.contains(c));
    }

    let mut loaded = Loaded {
        config,
        provenance,
        paths,
        initial_filter: overrides.filter.clone(),
    };
    apply_overrides(&mut loaded, overrides);
    Ok(loaded)
}

fn read_file(
    path: &Path,
    source: super::paths::ConfigSource,
    provenance: &mut Provenance,
) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        // A file the user named explicitly and that is not there is a mistake worth
        // reporting. A missing XDG default just means "use the built-in defaults".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !source.is_explicit() => {
            return Ok(Config::default());
        }
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    // Parsed twice on purpose: once untyped to learn which keys the file actually sets,
    // once typed to get `deny_unknown_fields` and real error messages. Config files are
    // small enough that the second parse is not worth engineering around.
    let document: toml::Value = toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(e),
    })?;
    provenance.record_document(&document);

    toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(e),
    })
}

fn apply_overrides(loaded: &mut Loaded, overrides: &Overrides) {
    if let Some(secs) = overrides.refresh_secs {
        loaded.config.refresh.interval_secs = secs;
        loaded.provenance.set("refresh.interval_secs", Source::Flag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::Env;
    use crate::config::schema::{Column, Scope};
    use std::path::PathBuf;

    struct Fixture {
        _tmp: tempfile::TempDir,
        home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("home");
            std::fs::create_dir_all(home.join(".config/mrq")).unwrap();
            Self { _tmp: tmp, home }
        }

        fn write_config(&self, body: &str) -> PathBuf {
            let path = self.home.join(".config/mrq/config.toml");
            std::fs::write(&path, body).unwrap();
            path
        }

        fn env(&self) -> Env {
            Env {
                home: Some(self.home.clone()),
                ..Env::default()
            }
        }

        fn load(&self, overrides: &Overrides) -> Result<Loaded, ConfigError> {
            let paths = Paths::resolve(&self.env(), None).unwrap();
            load(paths, overrides)
        }
    }

    #[test]
    fn no_config_file_loads_the_built_in_defaults() {
        let fx = Fixture::new();
        let loaded = fx.load(&Overrides::default()).unwrap();

        assert_eq!(loaded.config, Config::default());
        assert_eq!(
            loaded.provenance.iter().count(),
            0,
            "nothing came from a file"
        );
        assert_eq!(
            loaded.provenance.get("refresh.interval_secs"),
            Source::Default
        );
    }

    #[test]
    fn file_values_are_attributed_to_the_file() {
        let fx = Fixture::new();
        fx.write_config("[refresh]\ninterval_secs = 600\n");
        let loaded = fx.load(&Overrides::default()).unwrap();

        assert_eq!(loaded.config.refresh.interval_secs, 600);
        assert_eq!(loaded.provenance.get("refresh.interval_secs"), Source::File);
        assert_eq!(
            loaded.provenance.get("refresh.jitter_secs"),
            Source::Default,
            "a key the file did not mention is still a default"
        );
    }

    /// Flags layer over file values, and `mrq check` must be able to say so.
    #[test]
    fn flags_override_the_file_and_are_attributed_to_the_flag() {
        let fx = Fixture::new();
        fx.write_config("[refresh]\ninterval_secs = 600\n");

        let loaded = fx
            .load(&Overrides {
                refresh_secs: Some(45),
                filter: Some("Reviewing".into()),
            })
            .unwrap();

        assert_eq!(loaded.config.refresh.interval_secs, 45);
        assert_eq!(loaded.provenance.get("refresh.interval_secs"), Source::Flag);
        assert_eq!(loaded.initial_filter.as_deref(), Some("Reviewing"));
    }

    #[test]
    fn array_of_tables_keys_are_attributed_per_entry() {
        let fx = Fixture::new();
        fx.write_config(
            r#"
[[filter]]
name = "Assigned"
scope = "assigned"

[[filter]]
name = "Platform"
scope = "group"
path = "acme/platform"
"#,
        );
        let loaded = fx.load(&Overrides::default()).unwrap();

        assert_eq!(loaded.config.filters.len(), 2);
        assert_eq!(loaded.config.filters[1].scope, Scope::Group);
        assert_eq!(loaded.provenance.get("filter[0].name"), Source::File);
        assert_eq!(loaded.provenance.get("filter[1].path"), Source::File);
        assert_eq!(
            loaded.provenance.get("filter[1].max_results"),
            Source::Default,
        );
    }

    #[test]
    fn a_parse_error_names_the_file_and_the_key() {
        let fx = Fixture::new();
        let path = fx.write_config("[ui]\nshow_draft = true\n");
        let err = fx.load(&Overrides::default()).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains("show_draft"), "{msg}");
    }

    /// A file the user named explicitly and that is absent is an error;
    /// silently using defaults would leave them wondering why nothing took effect.
    #[test]
    fn an_explicitly_named_missing_file_is_an_error() {
        let fx = Fixture::new();
        let missing = fx.home.join("nowhere.toml");
        let paths = Paths::resolve(&fx.env(), Some(&missing)).unwrap();

        let err = load(paths, &Overrides::default()).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("nowhere.toml"), "{err}");
    }

    #[test]
    fn source_labels_are_stable() {
        assert_eq!(Source::Default.as_str(), "default");
        assert_eq!(Source::File.as_str(), "file");
        assert_eq!(Source::Env.as_str(), "env");
        assert_eq!(Source::Flag.as_str(), "flag");
    }

    #[test]
    fn provenance_lists_only_non_default_keys_in_path_order() {
        let fx = Fixture::new();
        fx.write_config("[gitlab]\nurl = \"https://git.example.com\"\n\n[ui]\nmouse = true\n");
        let loaded = fx.load(&Overrides::default()).unwrap();

        let keys: Vec<&str> = loaded.provenance.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["gitlab", "gitlab.url", "ui", "ui.mouse"]);
    }

    fn entry<'a>(
        entries: &'a [(String, Source, String)],
        key: &str,
    ) -> &'a (String, Source, String) {
        entries
            .iter()
            .find(|(k, ..)| k == key)
            .unwrap_or_else(|| panic!("no entry for {key}, have: {entries:?}"))
    }

    #[test]
    fn resolved_entries_reports_every_leaf_with_its_source() {
        let fx = Fixture::new();
        fx.write_config("[refresh]\ninterval_secs = 600\n");
        let loaded = fx.load(&Overrides::default()).unwrap();

        let entries = resolved_entries(&loaded.config, &loaded.provenance);

        assert_eq!(
            entry(&entries, "refresh.interval_secs"),
            &(
                "refresh.interval_secs".to_owned(),
                Source::File,
                "600".to_owned()
            )
        );
        assert_eq!(
            entry(&entries, "refresh.jitter_secs"),
            &(
                "refresh.jitter_secs".to_owned(),
                Source::Default,
                "15".to_owned()
            ),
            "a key nobody set is still reported, at its built-in default"
        );
    }

    #[test]
    fn resolved_entries_attributes_flag_overrides() {
        let fx = Fixture::new();
        let loaded = fx
            .load(&Overrides {
                refresh_secs: Some(45),
                filter: None,
            })
            .unwrap();

        let entries = resolved_entries(&loaded.config, &loaded.provenance);
        assert_eq!(
            entry(&entries, "refresh.interval_secs"),
            &(
                "refresh.interval_secs".to_owned(),
                Source::Flag,
                "45".to_owned()
            )
        );
    }

    /// A filter with no `path` (every currentUser scope) must not crash the walk: the
    /// field serializes to nothing at all, not to an empty string.
    #[test]
    fn resolved_entries_skips_unset_optional_fields() {
        let fx = Fixture::new();
        let loaded = fx.load(&Overrides::default()).unwrap();

        let entries = resolved_entries(&loaded.config, &loaded.provenance);
        assert!(
            entries.iter().all(|(k, ..)| k != "filter[0].path"),
            "{entries:?}"
        );
        assert!(
            entries.iter().all(|(k, ..)| k != "gitlab.token"),
            "no token configured, so no line to redact"
        );
    }

    /// A literal `token` in the file is a resolved value like any other; redacting it is
    /// `mrq check`'s job at display time, not this function's.
    #[test]
    fn resolved_entries_reports_a_configured_token_in_the_clear() {
        let fx = Fixture::new();
        fx.write_config("[gitlab]\ntoken = \"glpat-secret\"\n");
        let loaded = fx.load(&Overrides::default()).unwrap();

        let entries = resolved_entries(&loaded.config, &loaded.provenance);
        assert_eq!(
            entry(&entries, "gitlab.token"),
            &(
                "gitlab.token".to_owned(),
                Source::File,
                "glpat-secret".to_owned()
            )
        );
    }

    #[test]
    fn resolved_entries_addresses_array_of_tables_filters_per_entry() {
        let fx = Fixture::new();
        fx.write_config(
            r#"
[[filter]]
name = "Assigned"
scope = "assigned"

[[filter]]
name = "Platform"
scope = "group"
path = "acme/platform"
"#,
        );
        let loaded = fx.load(&Overrides::default()).unwrap();

        let entries = resolved_entries(&loaded.config, &loaded.provenance);
        assert_eq!(
            entry(&entries, "filter[1].path"),
            &(
                "filter[1].path".to_owned(),
                Source::File,
                "acme/platform".to_owned()
            )
        );
        assert_eq!(
            entry(&entries, "filter[0].max_results"),
            &(
                "filter[0].max_results".to_owned(),
                Source::Default,
                "100".to_owned()
            )
        );
    }

    /// A config predating `approver`/`reviewer` sets its own `columns` and never mentions
    /// `wide_columns`. It must not inherit the shipped default `wide_columns`, or it fails
    /// validation with "approver is not in ui.columns" for a key it never wrote.
    #[test]
    fn a_pre_existing_columns_list_does_not_inherit_the_default_wide_columns() {
        use crate::config::validate::validate;

        let fx = Fixture::new();
        fx.write_config(
            "[ui]\ncolumns = [\"approved\", \"author\", \"repo\", \"title\", \"pipeline\", \
             \"assigned\", \"age\", \"updated\", \"diff\"]\n",
        );
        let mut loaded = fx.load(&Overrides::default()).unwrap();

        assert!(
            loaded.config.ui.wide_columns.is_empty(),
            "neither approver nor reviewer is in the user's columns: {:?}",
            loaded.config.ui.wide_columns
        );
        validate(&mut loaded.config).expect("a pre-existing config must still load");
    }

    /// A user who does write `wide_columns` gets exactly what they asked for, not a
    /// merge with the shipped default.
    #[test]
    fn an_explicit_wide_columns_is_left_untouched() {
        let fx = Fixture::new();
        fx.write_config("[ui]\nwide_columns = [\"age\"]\n");
        let loaded = fx.load(&Overrides::default()).unwrap();

        assert_eq!(loaded.config.ui.wide_columns, vec![Column::Age]);
    }
}
