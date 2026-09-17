//! The config layer end to end, from TOML text and a synthetic environment.
//!
//! The per-module tests build values in code and check one rule each; this
//! drives `paths` → `load` → `validate` → `keymap` → `token` together, starting from the
//! bytes a user would actually write. The seam that needs it is the one no unit test
//! covers: a rule that holds when you construct the struct but is unreachable from a file,
//! or an error that is correct but arrives without the file path and key that make it
//! actionable.
//!
//! # Nothing here reads the process environment
//!
//! [`Env`] and [`TokenEnv`] are injected. The harness runs tests as threads of one
//! process, so a test that called `set_var` would race every other test that reads the
//! same variable and pass or fail on scheduling. Every case below is a pure function of
//! its inputs, and no case needs a token or a network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::init::DEFAULT_CONFIG;
use crate::config::keymap::{self, Action, Keymap};
use crate::config::load::{self, Overrides, Source};
use crate::config::paths::{Env, Paths};
use crate::config::schema::Config;
use crate::config::token::{self, TokenEnv, TokenSource};
use crate::config::validate::{self, Clamped};
use crate::error::ConfigError;

// ------------------------------------------------------------------------------ harness

/// A throwaway `$HOME` with the four config locations available.
struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: Env,
    flag: Option<PathBuf>,
}

/// Everything the config layer produces for one invocation.
#[derive(Debug)]
struct Outcome {
    config: Config,
    keymap: Keymap,
    clamps: Vec<Clamped>,
    provenance: load::Provenance,
    config_file: Option<PathBuf>,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let env = Env {
            home: Some(root.join("home")),
            ..Env::default()
        };
        Self {
            _tmp: tmp,
            root,
            env,
            flag: None,
        }
    }

    /// Write a config file and point `--config` at it.
    fn with_flag_config(mut self, body: &str) -> Self {
        let path = self.write(&self.root.join("flag/config.toml"), body);
        self.flag = Some(path);
        self
    }

    /// Write a config file and point `$MRQ_CONFIG` at it.
    fn with_env_config(mut self, body: &str) -> Self {
        let path = self.write(&self.root.join("env/config.toml"), body);
        self.env.mrq_config = Some(path);
        self
    }

    /// Write a config file under `$XDG_CONFIG_HOME` and set the variable.
    fn with_xdg_config(mut self, body: &str) -> Self {
        let xdg = self.root.join("xdg");
        self.write(&xdg.join("mrq/config.toml"), body);
        self.env.xdg_config_home = Some(xdg);
        self
    }

    /// Set `$XDG_CONFIG_HOME` to a directory with no config file in it, which must
    /// fall through to `$HOME/.config` rather than stop the search.
    fn with_empty_xdg_dir(mut self) -> Self {
        let xdg = self.root.join("empty-xdg");
        std::fs::create_dir_all(&xdg).unwrap();
        self.env.xdg_config_home = Some(xdg);
        self
    }

    /// Write `$HOME/.config/mrq/config.toml`.
    fn with_home_config(self, body: &str) -> Self {
        let path = self
            .env
            .home
            .clone()
            .unwrap()
            .join(".config/mrq/config.toml");
        self.write(&path, body);
        self
    }

    fn write(&self, path: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        path.to_path_buf()
    }

    /// The full pipeline. `Err` is whatever the first stage to reject produced, which is
    /// what the user sees.
    fn resolve(&self) -> Result<Outcome, ConfigError> {
        self.resolve_with(&Overrides::default())
    }

    fn resolve_with(&self, overrides: &Overrides) -> Result<Outcome, ConfigError> {
        let paths = Paths::resolve(&self.env, self.flag.as_deref())?;
        let config_file = paths.config_file().map(Path::to_path_buf);
        let mut loaded = load::load(paths, overrides)?;

        let clamps = validate::validate(&mut loaded.config)?;
        let keymap = keymap::resolve(&loaded.config.keys)?;

        Ok(Outcome {
            config: loaded.config,
            keymap,
            clamps,
            provenance: loaded.provenance,
            config_file,
        })
    }

    fn error(&self) -> String {
        match self.resolve() {
            Ok(_) => panic!("this configuration should have been rejected"),
            Err(err) => err.to_string(),
        }
    }
}

/// A single-table document, so a case reads as the one key it is about.
fn doc(body: &str) -> String {
    body.trim_start_matches('\n').to_owned()
}

// ----------------------------------------------------------------------------- defaults

/// The shipped file is entirely comments, so running with it and running with no config
/// file at all have to land on exactly the same configuration — otherwise the template is
/// making choices on the user's behalf without saying so.
#[test]
fn no_config_file_resolves_identically_to_the_shipped_default() {
    let with_file = World::new()
        .with_home_config(DEFAULT_CONFIG)
        .resolve()
        .unwrap();
    let without = World::new().resolve().unwrap();

    assert_eq!(without.config, with_file.config);
    assert_eq!(without.keymap, with_file.keymap);
    assert!(without.clamps.is_empty());
    assert_eq!(
        without.provenance.iter().count(),
        0,
        "nothing was configured, so nothing is attributed to a file"
    );
}

/// The shipped file is a template: its uncommented content must be exactly the tables and
/// the one default filter, or it is making choices on the user's behalf.
#[test]
fn the_shipped_default_config_only_uncomments_the_default_filter() {
    let uncommented: Vec<&str> = DEFAULT_CONFIG
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    assert_eq!(
        uncommented,
        [
            "[gitlab]",
            "[refresh]",
            "[ui]",
            "[skin]",
            "[skin.colors]",
            "[sort]",
            "[notifications]",
            "[browser]",
            "[[filter]]",
            "name = \"Assigned\"",
            "scope = \"assigned\"",
            "[keys]",
        ]
    );
}

// --------------------------------------------------------------------- rejection: typos

/// The error has to carry the file path and the offending key: a parse
/// failure that names neither leaves the user with several files and no idea which.
#[test]
fn an_unknown_key_is_rejected_and_names_the_file_and_the_key() {
    let world = World::new().with_home_config(&doc("
[ui]
show_draft = true
"));
    let err = world.error();

    assert!(err.contains("show_draft"), "{err}");
    assert!(err.contains("config.toml"), "{err}");
}

#[test]
fn every_shape_of_typo_is_rejected() {
    for (body, key) in [
        ("[ui]\nshow_draft = true\n", "show_draft"),
        ("[gitlab]\ntoken_cmd = \"x\"\n", "token_cmd"),
        ("[refresh]\ninterval = 60\n", "interval"),
        ("[notifications]\non_new_mrs = true\n", "on_new_mrs"),
        ("[[filter]]\nname = \"a\"\nlable = \"x\"\n", "lable"),
        ("[ui]\ntheme = \"solarized\"\n", "solarized"),
        ("[unknown]\nx = 1\n", "unknown"),
    ] {
        let err = World::new().with_home_config(body).error();
        assert!(err.contains(key), "`{key}` missing from: {err}");
    }
}

/// An unknown *action* in `[keys]` parses as TOML — the table is a free-form map — so it
/// can only be caught downstream, and the message has to list what is valid.
#[test]
fn an_unknown_key_action_is_rejected_with_the_valid_names() {
    let world = World::new().with_home_config(&doc("
[keys]
togle_drafts = [\"x\"]
"));
    let err = world.error();

    assert!(err.contains("togle_drafts"), "{err}");
    assert!(err.contains("toggle_drafts"), "should list valid: {err}");
}

// --------------------------------------------------------------- rejection: the conflict list

/// A key claimed by two actions is fatal, and both actions must be named or
/// the user cannot tell what else needs rebinding.
#[test]
fn a_duplicate_keybind_is_rejected_and_names_both_actions() {
    let world = World::new().with_home_config(&doc("
[keys]
toggle_drafts = [\"o\"]
"));

    match world.resolve().unwrap_err() {
        ConfigError::DuplicateKeybind { key, first, second } => {
            assert_eq!(key, "o");
            let both = [first.as_str(), second.as_str()];
            assert!(both.contains(&"open_mr"), "{both:?}");
            assert!(both.contains(&"toggle_drafts"), "{both:?}");
        }
        other => panic!("expected DuplicateKeybind, got {other:?}"),
    }
}

/// A collision between two keys the *user* bound, neither of which is a default.
#[test]
fn a_duplicate_keybind_between_two_user_actions_is_rejected() {
    let world = World::new().with_home_config(&doc("
[keys]
copy_url = [\"alt-c\"]
copy_branch = [\"alt-c\"]
"));
    assert!(
        matches!(
            world.resolve().unwrap_err(),
            ConfigError::DuplicateKeybind { .. }
        ),
        "a user-only collision is still fatal"
    );
}

/// Below 30 seconds is a hard error and not a clamp, because silently
/// correcting it would leave the user believing they refresh every 5 seconds.
#[test]
fn an_interval_below_thirty_seconds_is_rejected_not_clamped() {
    for secs in [0, 1, 29] {
        let world = World::new().with_home_config(&format!("[refresh]\ninterval_secs = {secs}\n"));
        let err = world.error();
        assert!(err.contains("interval_secs"), "{err}");
        assert!(err.contains("30"), "should name the minimum: {err}");
    }

    let ok = World::new()
        .with_home_config("[refresh]\ninterval_secs = 30\n")
        .resolve()
        .unwrap();
    assert_eq!(
        ok.config.refresh.interval_secs, 30,
        "the floor itself is fine"
    );
}

/// `--refresh-secs` reaches the same floor. A flag that bypassed it would make the rule
/// advisory, and the flag is the convenient way to hammer an instance.
#[test]
fn the_interval_floor_also_applies_to_the_flag() {
    let world = World::new();
    let err = world
        .resolve_with(&Overrides {
            refresh_secs: Some(5),
            ..Overrides::default()
        })
        .unwrap_err()
        .to_string();
    assert!(err.contains("interval_secs"), "{err}");
}

/// The single most likely configuration mistake: GitLab's GraphQL API has no
/// instance-wide merge-request search, so a label filter needs a group scope. The
/// message has to say that, because "not supported" on its own leaves the user stuck.
#[test]
fn labels_on_a_current_user_scope_are_rejected_with_the_alternative() {
    for scope in ["assigned", "review_requested", "authored"] {
        let world = World::new().with_home_config(&format!(
            "[[filter]]\nname = \"Mine\"\nscope = \"{scope}\"\nlabels = [\"team::platform\"]\n"
        ));
        let err = world.error();

        assert!(err.contains("labels"), "{err}");
        assert!(err.contains(scope), "{err}");
        assert!(err.contains("group"), "must name the alternative: {err}");
    }
}

/// Every one of the group/project-only arguments, not just `labels`, and all at once so
/// the user fixes the filter in one pass rather than one key per run.
#[test]
fn every_root_only_argument_on_a_current_user_scope_is_named_at_once() {
    let world = World::new().with_home_config(&doc("
[[filter]]
name = \"Mine\"
scope = \"assigned\"
labels = [\"a\"]
not_labels = [\"wip\"]
author = \"someuser\"
assignee = \"someuser\"
reviewer = \"someuser\"
milestone = \"24.Q3\"
target_branch = \"main\"
updated_after_days = 30
"));
    let err = world.error();

    for key in [
        "labels",
        "not_labels",
        "author",
        "assignee",
        "reviewer",
        "milestone",
        "target_branch",
        "updated_after_days",
    ] {
        assert!(err.contains(key), "`{key}` missing from: {err}");
    }
}

#[test]
fn a_group_or_project_scope_without_a_path_is_rejected() {
    for scope in ["group", "project"] {
        let world = World::new()
            .with_home_config(&format!("[[filter]]\nname = \"X\"\nscope = \"{scope}\"\n"));
        let err = world.error();

        assert!(err.contains("path"), "{err}");
        assert!(err.contains(scope), "{err}");
    }

    let ok = World::new()
        .with_home_config("[[filter]]\nname = \"X\"\nscope = \"group\"\npath = \"acme/platform\"\n")
        .resolve()
        .unwrap();
    assert_eq!(ok.config.filters[0].path.as_deref(), Some("acme/platform"));
}

/// An unknown column key is a serde failure rather than a semantic one, so the message
/// comes from the deserializer and must still list the alternatives.
#[test]
fn an_unknown_column_key_is_rejected_and_lists_the_real_ones() {
    let world = World::new().with_home_config(&doc("
[ui]
columns = [\"title\", \"reviewers\"]
"));
    let err = world.error();

    assert!(err.contains("reviewers"), "{err}");
    assert!(err.contains("pipeline"), "should list valid keys: {err}");

    // The neighbouring mistakes are semantic and reported separately.
    let dup = World::new()
        .with_home_config("[ui]\ncolumns = [\"title\", \"title\"]\n")
        .error();
    assert!(dup.contains("more than once"), "{dup}");

    let hidden = World::new()
        .with_home_config("[ui]\ncolumns = [\"title\"]\n\n[sort]\ncolumn = \"updated\"\n")
        .error();
    assert!(hidden.contains("sort.column"), "{hidden}");
}

/// Names address both a tab and a cache file, so a duplicate is ambiguous twice over.
#[test]
fn a_duplicate_filter_name_is_rejected() {
    let world = World::new().with_home_config(&doc("
[[filter]]
name = \"Mine\"
scope = \"assigned\"

[[filter]]
name = \"Mine\"
scope = \"authored\"
"));
    let err = world.error();

    assert!(err.contains("Mine"), "{err}");
    assert!(err.contains("more than one"), "{err}");
}

/// Two names that differ only in punctuation are distinct tabs but would share a cache
/// file if the slug did not disambiguate them; the rule is checked from a file because
/// that is the only place such a pair gets written.
#[test]
fn filters_differing_only_in_punctuation_are_accepted_with_distinct_cache_files() {
    let world = World::new().with_home_config(&doc("
[[filter]]
name = \"Team / Platform\"
scope = \"group\"
path = \"acme/platform\"

[[filter]]
name = \"Team: Platform\"
scope = \"group\"
path = \"acme/other\"
"));
    let outcome = world.resolve().unwrap();
    let slugs: Vec<String> = outcome.config.filters.iter().map(|f| f.slug()).collect();

    assert_ne!(slugs[0], slugs[1], "{slugs:?}");
}

#[test]
fn an_empty_filter_list_is_rejected() {
    // An explicit empty array, which is not the same as omitting the table: omitting it
    // leaves the built-in `Assigned` filter, and there would be no tab to show otherwise.
    let world = World::new().with_home_config("filter = []\n");
    assert!(world.error().contains("at least one"));
}

/// A config with several mistakes reports all of them, so the user does not
/// fix one and re-run to discover the next.
#[test]
fn several_semantic_mistakes_are_reported_together() {
    let world = World::new().with_home_config(&doc("
[gitlab]
url = \"gitlab.example.com\"

[refresh]
interval_secs = 5

[[filter]]
name = \"Mine\"
scope = \"assigned\"
labels = [\"a\"]
"));
    let err = world.error();

    assert!(err.contains("gitlab.url"), "{err}");
    assert!(err.contains("interval_secs"), "{err}");
    assert!(err.contains("labels"), "{err}");
    assert_eq!(err.matches("\n  - ").count(), 3, "one line each: {err}");
}

/// Out-of-range values that only affect the person who typed them clamp and warn, and the
/// warning has to survive to the caller — a silent clamp is indistinguishable from the
/// setting being ignored.
#[test]
fn out_of_range_values_clamp_with_a_warning_rather_than_failing() {
    let world = World::new().with_home_config(&doc("
[refresh]
interval_secs = 60
jitter_secs = 600

[[filter]]
name = \"Mine\"
scope = \"assigned\"
max_results = 5000
"));
    let outcome = world.resolve().unwrap();

    assert_eq!(outcome.config.refresh.jitter_secs, 60);
    assert_eq!(outcome.config.filters[0].max_results, 500);

    let keys: Vec<&str> = outcome.clamps.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(keys, ["refresh.jitter_secs", "filter[0].max_results"]);
}

// ----------------------------------------------------------- precedence: config location

/// Four config locations, first hit wins. Each is removed in turn so that the test
/// fails if any one of them is skipped, rather than only pinning the top of the list.
#[test]
fn the_four_config_locations_are_tried_in_spec_order() {
    let body = |name: &str| format!("[[filter]]\nname = \"{name}\"\nscope = \"assigned\"\n");

    let all = World::new()
        .with_flag_config(&body("flag"))
        .with_env_config(&body("env"))
        .with_xdg_config(&body("xdg"))
        .with_home_config(&body("home"));
    assert_eq!(filter_name(&all), "flag");

    let no_flag = World::new()
        .with_env_config(&body("env"))
        .with_xdg_config(&body("xdg"))
        .with_home_config(&body("home"));
    assert_eq!(filter_name(&no_flag), "env");

    let no_env = World::new()
        .with_xdg_config(&body("xdg"))
        .with_home_config(&body("home"));
    assert_eq!(filter_name(&no_env), "xdg");

    let home_only = World::new().with_home_config(&body("home"));
    assert_eq!(filter_name(&home_only), "home");

    let none = World::new().resolve().unwrap();
    assert_eq!(none.config_file, None, "and nothing is not an error");
    assert_eq!(none.config.filters[0].name, "Assigned", "built-in default");
}

fn filter_name(world: &World) -> String {
    world.resolve().unwrap().config.filters[0].name.clone()
}

/// `$XDG_CONFIG_HOME` and `$HOME/.config` are separate candidates, so a set-but-empty
/// `XDG_CONFIG_HOME` falls through instead of ending the search.
#[test]
fn an_xdg_config_home_with_no_file_falls_through_to_home() {
    let world = World::new()
        .with_empty_xdg_dir()
        .with_home_config("[[filter]]\nname = \"home\"\nscope = \"assigned\"\n");

    assert_eq!(filter_name(&world), "home");
}

/// A file the user named explicitly and that is absent is an error. Falling back to the
/// defaults would leave them wondering why their settings did nothing.
#[test]
fn an_explicitly_named_missing_file_is_an_error_but_a_missing_xdg_one_is_not() {
    let mut world = World::new();
    world.flag = Some(world.root.join("nowhere.toml"));
    let err = world.resolve().unwrap_err();
    assert!(matches!(err, ConfigError::Io { .. }), "{err:?}");
    assert!(err.to_string().contains("nowhere.toml"), "{err}");

    let mut world = World::new();
    world.env.mrq_config = Some(world.root.join("also-nowhere.toml"));
    assert!(matches!(
        world.resolve().unwrap_err(),
        ConfigError::Io { .. }
    ));

    assert!(
        World::new().resolve().is_ok(),
        "but an absent XDG file is not"
    );
}

/// `mrq check` prints where every value came from, so the provenance has to
/// distinguish all four sources on one load.
#[test]
fn provenance_distinguishes_defaults_files_and_flags() {
    let world = World::new().with_home_config(&doc("
[refresh]
interval_secs = 600

[ui]
mouse = true
"));
    let outcome = world
        .resolve_with(&Overrides {
            refresh_secs: Some(120),
            filter: None,
        })
        .unwrap();

    assert_eq!(outcome.config.refresh.interval_secs, 120);
    assert_eq!(
        outcome.provenance.get("refresh.interval_secs"),
        Source::Flag,
        "the flag layered over the file"
    );
    assert_eq!(outcome.provenance.get("ui.mouse"), Source::File);
    assert_eq!(
        outcome.provenance.get("refresh.jitter_secs"),
        Source::Default,
        "a key nothing mentioned"
    );
}

// -------------------------------------------------------------- precedence: token source

/// Four token sources, first hit wins. Each is removed in turn, so the test fails if
/// any single step of the chain is skipped.
#[test]
fn the_four_token_sources_are_tried_in_spec_order() {
    let world = World::new().with_home_config(&doc("
[gitlab]
token = \"from-file\"
token_command = \"printf from-command\"
"));
    let config = world.resolve().unwrap().config;
    let path = world.resolve().unwrap().config_file;

    let env = |mrq: Option<&str>, gl: Option<&str>| TokenEnv {
        mrq_token: mrq.map(str::to_owned),
        gitlab_token: gl.map(str::to_owned),
    };
    let resolve = |env: &TokenEnv| {
        token::resolve(&config.gitlab, env, path.as_deref()).expect("a token is available")
    };

    let both = resolve(&env(Some("from-mrq"), Some("from-gitlab")));
    assert_eq!(both.token.expose(), "from-mrq");
    assert_eq!(both.token.source(), TokenSource::MrqTokenEnv);

    let no_mrq = resolve(&env(None, Some("from-gitlab")));
    assert_eq!(no_mrq.token.expose(), "from-gitlab");
    assert_eq!(no_mrq.token.source(), TokenSource::GitlabTokenEnv);

    let neither = resolve(&TokenEnv::default());
    assert_eq!(neither.token.expose(), "from-command");
    assert_eq!(neither.token.source(), TokenSource::TokenCommand);

    let mut file_only = config.gitlab.clone();
    file_only.token_command = None;
    let got = token::resolve(&file_only, &TokenEnv::default(), path.as_deref()).unwrap();
    assert_eq!(got.token.expose(), "from-file");
    assert_eq!(got.token.source(), TokenSource::ConfigFile);
}

/// An empty variable is how people unset one in a shell profile. Taken literally it would
/// produce an empty `Bearer` header and a 401 that explains nothing.
#[test]
fn empty_token_variables_fall_through_to_the_next_source() {
    let world = World::new().with_home_config("[gitlab]\ntoken = \"from-file\"\n");
    let outcome = world.resolve().unwrap();

    let got = token::resolve(
        &outcome.config.gitlab,
        &TokenEnv {
            mrq_token: Some("   ".into()),
            gitlab_token: Some("\n".into()),
        },
        outcome.config_file.as_deref(),
    )
    .unwrap();
    assert_eq!(got.token.expose(), "from-file");
}

/// Fatal, and the message names all four sources so a user who set the
/// wrong one of them can see which mrq actually reads.
#[test]
fn no_token_anywhere_is_fatal_and_names_every_source() {
    let outcome = World::new().resolve().unwrap();
    let err = token::resolve(&outcome.config.gitlab, &TokenEnv::default(), None).unwrap_err();

    assert!(matches!(err, ConfigError::MissingToken));
    let msg = err.to_string();
    for source in ["MRQ_TOKEN", "GITLAB_TOKEN", "token_command", "token"] {
        assert!(msg.contains(source), "`{source}` missing from: {msg}");
    }
}

/// A literal token in a file others can read warns and continues. The
/// warning is driven from a real file here because the mode is the whole point.
#[test]
fn a_group_readable_file_with_a_literal_token_warns_and_continues() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::new().with_home_config("[gitlab]\ntoken = \"glpat-SECRET\"\n");
    let outcome = world.resolve().unwrap();
    let path = outcome.config_file.clone().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let got = token::resolve(&outcome.config.gitlab, &TokenEnv::default(), Some(&path)).unwrap();

    assert_eq!(got.token.expose(), "glpat-SECRET", "resolution succeeds");
    assert_eq!(got.warnings.len(), 1);
    let rendered = got.warnings[0].to_string();
    assert!(rendered.contains("0644"), "{rendered}");
    assert!(!rendered.contains("SECRET"), "warning leaked the token");

    // And the token cannot reach the log through the config it lives in.
    assert!(
        !format!("{:?}", got.token).contains("SECRET"),
        "Debug leaked the token"
    );
}

/// A `token_command` that fails is a startup error, and the tool's own stderr says why
/// better than mrq can — a declined keychain prompt and a missing entry look identical
/// otherwise.
#[test]
fn a_failing_token_command_is_fatal_and_quotes_its_stderr() {
    let world = World::new().with_home_config(&doc("
[gitlab]
token_command = \"echo 'no such keychain item' >&2; exit 3\"
"));
    let outcome = world.resolve().unwrap();
    let err = token::resolve(&outcome.config.gitlab, &TokenEnv::default(), None).unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("status 3"), "{msg}");
    assert!(msg.contains("no such keychain item"), "{msg}");
}

// ------------------------------------------------------------------------------- keymap

fn keymap_from(body: &str) -> Keymap {
    World::new()
        .with_home_config(body)
        .resolve()
        .unwrap()
        .keymap
}

fn specs_for(map: &Keymap, action: Action) -> Vec<String> {
    map.keys_for(action).iter().map(|k| k.to_spec()).collect()
}

/// The documented key-spec grammar. These are the cases that a naive `split('-')`
/// gets wrong, driven through a real `[keys]` table because that is where they get
/// written.
#[test]
fn the_key_spec_grammar_accepts_its_documented_edge_cases() {
    let map = keymap_from(&doc("
[keys]
quit = [\"ctrl-alt-super-q\"]
refresh = [\"F5\"]
refresh_visible = [\"-\"]
log_menu = [\"ctrl--\"]
help = [\"space\"]
top = [\"ALT-x\"]
bottom = [\"shift-tab\"]
prev_filter = [\"backspace\"]
search = [\"Ctrl-Shift-F\"]
"));

    assert_eq!(specs_for(&map, Action::Quit), ["ctrl-alt-super-q"]);
    assert_eq!(specs_for(&map, Action::Refresh), ["f5"], "case-insensitive");
    assert_eq!(specs_for(&map, Action::RefreshVisible), ["-"], "bare minus");
    assert_eq!(specs_for(&map, Action::LogMenu), ["ctrl--"]);
    assert_eq!(specs_for(&map, Action::Help), ["space"]);
    assert_eq!(specs_for(&map, Action::Top), ["alt-x"]);
    assert_eq!(
        specs_for(&map, Action::Bottom),
        ["shift-tab"],
        "shift survives on a named key"
    );
    assert_eq!(specs_for(&map, Action::PrevFilter), ["backspace"]);
    assert_eq!(
        specs_for(&map, Action::Search),
        ["ctrl-F"],
        "shift folds into the character for character keys"
    );
}

/// `shift-S` and `S` are the same binding, so a user who writes one form
/// cannot collide with a default written in the other.
#[test]
fn shift_and_uppercase_are_the_same_binding_across_the_file() {
    let map = keymap_from("[keys]\nsort_menu = [\"S\"]\n");
    assert_eq!(specs_for(&map, Action::SortMenu), ["S"]);

    // `shift-S` is already the default for sort_menu, so binding `S` elsewhere collides.
    let err = World::new()
        .with_home_config("[keys]\ncopy_url = [\"S\"]\n")
        .resolve()
        .unwrap_err();
    assert!(
        matches!(err, ConfigError::DuplicateKeybind { .. }),
        "{err:?}"
    );
}

#[test]
fn an_unparseable_spec_is_rejected_and_names_the_offending_value() {
    for (spec, needle) in [
        ("hyper-x", "unknown modifier"),
        ("ctrl-nope", "not a single character"),
        ("f13", "not a single character"),
        ("f0", "not a single character"),
        ("\"\"", "empty"),
    ] {
        let quoted = if spec.starts_with('"') {
            spec.to_owned()
        } else {
            format!("\"{spec}\"")
        };
        let err = World::new()
            .with_home_config(&format!("[keys]\nquit = [{quoted}]\n"))
            .error();
        assert!(err.contains(needle), "`{spec}` -> {err}");
    }
}

/// The table merges *over* the defaults, so a partial table only overrides
/// what it names.
#[test]
fn a_partial_keys_table_merges_over_the_defaults() {
    let map = keymap_from("[keys]\nquit = [\"ctrl-q\"]\n");

    assert_eq!(specs_for(&map, Action::Quit), ["ctrl-q"]);
    assert_eq!(
        specs_for(&map, Action::Down),
        ["j", "down"],
        "untouched actions keep their defaults"
    );
    assert_eq!(specs_for(&map, Action::Help), ["?", "f1"]);

    // And every other action still resolves, rather than the table replacing the map.
    for action in Action::ALL {
        let defaults = keymap::DEFAULT_BINDINGS
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, specs)| specs.len())
            .unwrap();
        if action != Action::Quit {
            assert_eq!(
                map.keys_for(action).len(),
                defaults,
                "{} lost its defaults",
                action.key()
            );
        }
    }
}

/// Rebinding replaces rather than appends, or a user could never remove a default — only
/// add to it — and `action = []` would be meaningless.
#[test]
fn rebinding_replaces_the_default_rather_than_adding_to_it() {
    let map = keymap_from("[keys]\ndown = [\"ctrl-n\"]\n");
    assert_eq!(specs_for(&map, Action::Down), ["ctrl-n"]);
    assert!(
        !specs_for(&map, Action::Down).contains(&"j".to_owned()),
        "the default must be gone"
    );
}

/// Setting an action to `[]` unbinds it.
#[test]
fn an_empty_list_unbinds_and_frees_the_key_for_another_action() {
    let map = keymap_from("[keys]\nquit = []\n");
    assert!(specs_for(&map, Action::Quit).is_empty());

    // Unbinding must actually release the key, not merely hide it from the help popup.
    let reused = keymap_from("[keys]\nquit = []\ntoggle_drafts = [\"q\"]\n");
    assert_eq!(specs_for(&reused, Action::ToggleDrafts), ["q"]);
    assert!(specs_for(&reused, Action::Quit).is_empty());
}

/// The conflict rule must not make swapping two keys impossible: rebinding both sides of
/// a collision in one file is legitimate.
#[test]
fn swapping_two_bindings_in_one_file_is_allowed() {
    let map = keymap_from(&doc("
[keys]
toggle_drafts = [\"o\"]
open_mr = [\"d\", \"enter\"]
"));

    assert_eq!(specs_for(&map, Action::ToggleDrafts), ["o"]);
    assert_eq!(specs_for(&map, Action::OpenMr), ["d", "enter"]);
}

/// The help popup is generated from the resolved map, so it cannot lie to a
/// user who has rebound something, and it must not advertise an unbound action.
#[test]
fn the_resolved_map_is_what_the_help_popup_reads() {
    let map = keymap_from("[keys]\nquit = [\"ctrl-q\"]\nhelp = []\n");

    assert_eq!(specs_for(&map, Action::Quit), ["ctrl-q"]);
    let app = map.bound_in(keymap::Category::Application);
    assert!(app.contains(&Action::Quit));
    assert!(
        !app.contains(&Action::Help),
        "unbound actions are not shown"
    );
    assert!(
        !app.contains(&Action::LogMenu),
        "and neither are the ones that ship unbound"
    );
}

/// The defaults are the keymap a user with no config file gets, so a conflict among them
/// would make mrq refuse to start with no config at all.
#[test]
fn the_shipped_defaults_resolve_with_no_config_file() {
    let map = World::new().resolve().unwrap().keymap;

    assert_eq!(specs_for(&map, Action::Quit), ["q", "ctrl-c"]);
    assert!(keymap::resolve(&BTreeMap::new()).is_ok());
}
