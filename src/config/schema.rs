//! The TOML configuration schema: one struct per table, mirrored with serde.
//!
//! Every struct carries `deny_unknown_fields`. A typo must be a hard error: a silently
//! ignored `show_draft = true` looks like a bug in `mrq` rather than a mistake in the
//! file, and the user has no way to tell the difference.
//!
//! Defaults live in `Default` impls rather than `#[serde(default = "...")]` helpers, so
//! the built-in configuration is one readable block per table and `mrq init-config` can
//! round-trip against it.
//!
//! Semantic validation — interval floors, scope/argument compatibility, column keys — is
//! *not* here. This module answers "does it parse"; `validate` answers "does it mean
//! anything".

use std::collections::BTreeMap;

use schemars::JsonSchema;

use crate::config::validate::NEVER_WIDE;
use serde::{Deserialize, Serialize};

/// A parsed configuration file, before semantic validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub gitlab: Gitlab,
    pub refresh: Refresh,
    pub ui: Ui,
    pub skin: Skin,
    pub sort: Sort,
    pub notifications: Notifications,
    pub browser: Browser,

    /// `[[filter]]` array-of-tables. Order defines tab order and the 1..9 shortcuts.
    #[serde(rename = "filter")]
    pub filters: Vec<Filter>,

    /// Raw `[keys]` table: action name to key specs. Left unparsed here because the
    /// grammar, the merge over defaults and duplicate detection are all one concern,
    /// handled in `keymap`.
    pub keys: BTreeMap<String, Vec<String>>,
}

/// The JSON Schema for the TOML configuration file, derived from this module's types.
///
/// Generated rather than hand-written, so it cannot drift from what the parser actually
/// accepts: `deny_unknown_fields` becomes `additionalProperties: false`, and every
/// `Default` impl becomes the schema's per-field default. `mrq schema` prints this for
/// editors to validate and autocomplete `config.toml` against.
pub fn json_schema() -> schemars::Schema {
    // TOML has no `null`: an absent key is how an `Option` is "unset", so the schema must
    // not offer `null` as a value.
    let mut value = serde_json::to_value(schemars::schema_for!(Config)).unwrap_or_default();
    strip_null(&mut value);
    constrain(&mut value);
    schemars::Schema::try_from(value).unwrap_or_default()
}

/// Removes `null` from every `type` list and from every default, recursively.
fn strip_null(value: &mut serde_json::Value) {
    use serde_json::Value;

    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|t| t != "null");
                if types.len() == 1 {
                    let only = types.remove(0);
                    map.insert("type".into(), only);
                }
            }
            if map.get("default").is_some_and(Value::is_null) {
                map.remove("default");
            }
            if let Some(Value::Object(default)) = map.get_mut("default") {
                default.retain(|_, v| !v.is_null());
            }
            // `default` values are data, not schemas: only recurse into the rest.
            for (key, child) in map.iter_mut() {
                if key != "default" && key != "const" && key != "enum" {
                    strip_null(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_null),
        _ => {}
    }
}

/// Arguments only the group, project and instance query roots accept; mirrors
/// [`Filter::root_only_args`].
const ROOT_ONLY_ARGS: [&str; 9] = [
    "labels",
    "not_labels",
    "author",
    "assignee",
    "reviewer",
    "has_reviewer",
    "milestone",
    "target_branch",
    "updated_after_days",
];

/// Adds the rules `validate` enforces as hard errors that the derive cannot see, so an
/// editor flags a file `mrq` would refuse to start with. Clamped values and cross-list
/// rules (`sort.column` in `ui.columns`, unique names) are deliberately left to `validate`.
fn constrain(root: &mut serde_json::Value) {
    use serde_json::{Value, json};

    use crate::config::keymap::Action;
    use crate::config::validate::MIN_INTERVAL_SECS;

    fn def<'a>(root: &'a mut Value, name: &str) -> &'a mut serde_json::Map<String, Value> {
        root.pointer_mut(&format!("/$defs/{name}"))
            .and_then(Value::as_object_mut)
            .unwrap_or_else(|| panic!("schema has no `{name}` definition"))
    }
    fn prop<'a>(
        root: &'a mut Value,
        name: &str,
        key: &str,
    ) -> &'a mut serde_json::Map<String, Value> {
        root.pointer_mut(&format!("/$defs/{name}/properties/{key}"))
            .and_then(Value::as_object_mut)
            .unwrap_or_else(|| panic!("schema has no `{name}.{key}` property"))
    }

    prop(root, "Refresh", "interval_secs").insert("minimum".into(), json!(MIN_INTERVAL_SECS));
    prop(root, "Gitlab", "timeout_secs").insert("minimum".into(), json!(1));
    prop(root, "Gitlab", "max_concurrent_requests").insert("minimum".into(), json!(1));
    prop(root, "Gitlab", "url").insert("pattern".into(), json!("^https?://"));

    prop(root, "Ui", "columns").insert("minItems".into(), json!(1));
    prop(root, "Filter", "columns").insert("minItems".into(), json!(1));
    prop(root, "Filter", "name").insert("pattern".into(), json!(r"\S"));
    if let Some(filter) = root
        .pointer_mut("/properties/filter")
        .and_then(Value::as_object_mut)
    {
        filter.insert("minItems".into(), json!(1));
    }

    let swatches: Vec<_> = crate::ui::palette::SWATCHES.to_vec();
    let colors = prop(root, "Skin", "colors");
    colors.insert("propertyNames".into(), json!({ "enum": swatches }));
    colors.insert(
        "additionalProperties".into(),
        json!({ "type": "string", "pattern": "^#?[0-9a-fA-F]{6}$" }),
    );

    let actions: Vec<_> = Action::ALL.iter().map(|a| a.key()).collect();
    if let Some(keys) = root
        .pointer_mut("/properties/keys")
        .and_then(Value::as_object_mut)
    {
        keys.insert("propertyNames".into(), json!({ "enum": actions }));
    }

    // `scope` defaults to `assigned`, so an absent `scope` behaves like a current-user one.
    let no_root_args: serde_json::Map<String, Value> = ROOT_ONLY_ARGS
        .iter()
        .map(|k| ((*k).to_owned(), Value::Bool(false)))
        .collect();
    def(root, "Filter").insert(
        "allOf".into(),
        json!([
            {
                "if": { "required": ["scope"], "properties": { "scope": { "enum": ["group", "project"] } } },
                "then": {
                    "required": ["path"],
                    "properties": { "path": { "pattern": r"^[^/\s]$|^[^/\s].*[^/]$" } }
                },
                "else": { "not": { "required": ["path"] } }
            },
            {
                "if": { "required": ["scope"], "properties": { "scope": { "const": "group" } } },
                "else": { "not": { "required": ["include_subgroups"] } }
            },
            { "not": { "required": ["reviewer", "has_reviewer"] } },
            {
                "if": { "anyOf": [
                    { "not": { "required": ["scope"] } },
                    { "properties": { "scope": { "enum": ["assigned", "review_requested", "authored"] } } }
                ] },
                "then": { "properties": no_root_args }
            }
        ]),
    );
    def(root, "Notifications").insert(
        "allOf".into(),
        json!([{
            "if": { "required": ["backend"], "properties": { "backend": { "const": "command" } } },
            "then": { "required": ["command"], "properties": { "command": { "pattern": r"\S" } } }
        }]),
    );
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gitlab: Gitlab::default(),
            refresh: Refresh::default(),
            ui: Ui::default(),
            skin: Skin::default(),
            sort: Sort::default(),
            notifications: Notifications::default(),
            browser: Browser::default(),
            // The default view is the merge requests assigned to me.
            filters: vec![Filter::named("Assigned", Scope::Assigned)],
            keys: BTreeMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------- [gitlab]

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Gitlab {
    pub url: String,
    /// Discouraged; the environment or `token_command` are preferred. Kept as a
    /// plain `String` here and wrapped in the redacting type at resolution time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_command: Option<String>,
    pub timeout_secs: u64,
    pub max_concurrent_requests: usize,
}

impl Default for Gitlab {
    fn default() -> Self {
        Self {
            url: "https://gitlab.com".into(),
            token: None,
            token_command: None,
            timeout_secs: 20,
            max_concurrent_requests: 4,
        }
    }
}

// --------------------------------------------------------------------------- [refresh]

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Refresh {
    pub interval_secs: u64,
    pub jitter_secs: u64,
    pub refresh_on_focus: bool,
    pub pause_when_unfocused: bool,
}

impl Default for Refresh {
    fn default() -> Self {
        Self {
            interval_secs: 300,
            jitter_secs: 15,
            refresh_on_focus: true,
            pause_when_unfocused: false,
        }
    }
}

// -------------------------------------------------------------------------------- [ui]

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Ui {
    /// Substitutes ASCII for every glyph, for terminals without a Unicode-capable font.
    /// Orthogonal to the colour skin.
    pub ascii: bool,
    pub show_drafts: bool,
    pub relative_times: bool,
    pub mouse: bool,
    pub set_terminal_title: bool,
    /// The columns, in display order. A `:wide` suffix (`"id:wide"`) hides that column until
    /// wide mode (`w`) is on. `approved`, `author`, `repo`, `title` and `pipeline` can
    /// never be wide-only — validated at config load. A `[[filter]]` can override the
    /// whole list with its own `columns`.
    pub columns: Vec<ColumnSpec>,
    /// Removed: kept only so `validate` can say what replaced it, instead of serde's
    /// bare "unknown field".
    #[serde(skip_serializing)]
    #[schemars(skip)]
    pub wide_columns: Option<Vec<Column>>,
    /// Starts with wide mode on, as if `w` had been pressed at launch.
    pub wide: bool,
    /// How the ASSIGNED column names people. `Yes`/`No` against whether you are an
    /// assignee, by default.
    pub assigned_display: PeopleDisplay,
    /// How the APPROVER column names people. The first approver's username, `+N` for the
    /// rest, `-` for none, by default.
    pub approver_display: PeopleDisplay,
    /// How the REVIEWER column names people. The first reviewer's username, `+N` for the
    /// rest, `-` for none, by default.
    pub reviewer_display: PeopleDisplay,
    /// Which column's text is the clickable link to the merge request, in a terminal
    /// that supports hyperlinks. `id` only links while the `id` column is on screen; `both` links the two, `none` turns hyperlinks off.
    pub link: LinkTarget,
}

/// The column(s) that carry the hyperlink to the merge request.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LinkTarget {
    Title,
    Id,
    Both,
    /// No hyperlinks at all.
    None,
}

impl LinkTarget {
    pub const fn columns(self) -> &'static [Column] {
        match self {
            Self::Title => &[Column::Title],
            Self::Id => &[Column::Id],
            Self::Both => &[Column::Id, Column::Title],
            Self::None => &[],
        }
    }
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            ascii: false,
            show_drafts: false,
            relative_times: true,
            mouse: false,
            set_terminal_title: true,
            columns: ColumnSpec::DEFAULT.to_vec(),
            wide_columns: None,
            wide: false,
            assigned_display: PeopleDisplay::YesNo,
            approver_display: PeopleDisplay::Username,
            reviewer_display: PeopleDisplay::Username,
            link: LinkTarget::Title,
        }
    }
}

/// How a people-naming column (`assigned`, `approver`, `reviewer`) renders who is there.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PeopleDisplay {
    /// Whether the current user is one of them.
    YesNo,
    /// The first person's username, `+N` for the rest, `-` for none.
    Username,
    /// The first person's trigram — initials, `Charles Billow` -> `CBI` — `-` for none.
    Trigram,
}

// ------------------------------------------------------------------------------ [skin]

/// Which palette the UI draws with, and any swatch the user wants to differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Skin {
    /// A built-in skin name, an alias, or `auto` for the shipped dark default.
    /// Validated against the built-in table at startup.
    pub name: String,
    /// Per-swatch overrides: swatch name to `#rrggbb`, applied over the named skin.
    pub colors: BTreeMap<String, String>,
}

impl Default for Skin {
    fn default() -> Self {
        Self {
            name: "catppuccin-mocha".into(),
            colors: BTreeMap::new(),
        }
    }
}

/// A table column. The variant order is not the display order — that comes from
/// `[ui].columns` — but it is the order used for the shipped default.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Approved,
    Author,
    Repo,
    Id,
    Title,
    Pipeline,
    Assigned,
    Approver,
    Reviewer,
    Age,
    Updated,
    Diff,
    Branch,
}

impl Column {
    /// Every column, including those not in the default list.
    pub const ALL: [Self; 13] = [
        Self::Approved,
        Self::Author,
        Self::Repo,
        Self::Id,
        Self::Title,
        Self::Pipeline,
        Self::Assigned,
        Self::Approver,
        Self::Reviewer,
        Self::Age,
        Self::Updated,
        Self::Diff,
        Self::Branch,
    ];

    pub const DEFAULT: [Self; 12] = [
        Self::Approved,
        Self::Author,
        Self::Repo,
        Self::Id,
        Self::Title,
        Self::Pipeline,
        Self::Assigned,
        Self::Approver,
        Self::Reviewer,
        Self::Age,
        Self::Updated,
        Self::Diff,
    ];

    /// The `[ui].columns` key for this column.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Author => "author",
            Self::Repo => "repo",
            Self::Id => "id",
            Self::Title => "title",
            Self::Pipeline => "pipeline",
            Self::Assigned => "assigned",
            Self::Approver => "approver",
            Self::Reviewer => "reviewer",
            Self::Age => "age",
            Self::Updated => "updated",
            Self::Diff => "diff",
            Self::Branch => "branch",
        }
    }

    /// The header text.
    ///
    /// `approved` has none: its column is just the checkmark, and a header only widens
    /// it for a label nobody needs to read twice.
    pub const fn header(self) -> &'static str {
        match self {
            Self::Approved => "",
            Self::Author => "AUTHOR",
            Self::Repo => "REPO",
            Self::Id => "ID",
            Self::Title => "TITLE",
            Self::Pipeline => "CI",
            Self::Assigned => "ASG",
            Self::Approver => "APPROVER",
            Self::Reviewer => "REVIEWER",
            Self::Age => "AGE",
            Self::Updated => "UPDATED",
            Self::Diff => "DIFF",
            Self::Branch => "BRANCH",
        }
    }
}

/// A column in a `columns` list: the column, plus whether it is wide-only (`"id:wide"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSpec {
    pub column: Column,
    /// Hidden until wide mode (`w`) is on.
    pub wide: bool,
}

impl ColumnSpec {
    const fn shown(column: Column) -> Self {
        Self {
            column,
            wide: false,
        }
    }

    const fn wide(column: Column) -> Self {
        Self { column, wide: true }
    }

    /// The shipped default list: `Column::DEFAULT`, with `id`, `approver`, `reviewer`
    /// and `diff` wide-only.
    pub const DEFAULT: [Self; 12] = [
        Self::shown(Column::Approved),
        Self::shown(Column::Author),
        Self::shown(Column::Repo),
        Self::wide(Column::Id),
        Self::shown(Column::Title),
        Self::shown(Column::Pipeline),
        Self::shown(Column::Assigned),
        Self::wide(Column::Approver),
        Self::wide(Column::Reviewer),
        Self::shown(Column::Age),
        Self::shown(Column::Updated),
        Self::wide(Column::Diff),
    ];

    /// The `columns` entry for this spec: `key`, or `key:wide`.
    pub fn spec(self) -> String {
        if self.wide {
            format!("{}:wide", self.column.key())
        } else {
            self.column.key().to_owned()
        }
    }
}

impl From<Column> for ColumnSpec {
    fn from(column: Column) -> Self {
        Self::shown(column)
    }
}

impl Serialize for ColumnSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.spec())
    }
}

impl<'de> Deserialize<'de> for ColumnSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error, IntoDeserializer};

        let raw = String::deserialize(deserializer)?;
        let (key, wide) = match raw.split_once(':') {
            None => (raw.as_str(), false),
            Some((key, "wide")) => (key, true),
            Some((_, suffix)) => {
                return Err(D::Error::custom(format!(
                    "invalid column suffix \":{suffix}\" in \"{raw}\": only \":wide\" (wide-only) is allowed"
                )));
            }
        };
        // Through `Column`'s own impl, so an unknown key lists the valid ones.
        let column = Column::deserialize(IntoDeserializer::<D::Error>::into_deserializer(key))?;
        Ok(Self { column, wide })
    }
}

impl JsonSchema for ColumnSpec {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ColumnSpec".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let keys = |wide: bool| {
            Column::ALL
                .into_iter()
                .filter(|c| NEVER_WIDE.contains(c) != wide)
                .map(Column::key)
                .collect::<Vec<_>>()
                .join("|")
        };
        schemars::json_schema!({
            "type": "string",
            "pattern": format!("^(({})|({})(:wide)?)$", keys(false), keys(true)),
            "description": "A column key, optionally suffixed `:wide` to show it only in wide mode.",
        })
    }
}

// ------------------------------------------------------------------------------ [sort]

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Sort {
    pub column: Column,
    pub order: Order,
    /// Sink drafts below non-drafts when drafts are shown.
    pub drafts_last: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            column: Column::Updated,
            order: Order::Desc,
            drafts_last: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Asc,
    Desc,
}

impl Order {
    #[must_use]
    pub const fn inverted(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

// --------------------------------------------------------------------- [notifications]

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
    pub backend: NotifyBackend,
    /// Used when `backend = "command"`; `{title}` and `{body}` are substituted.
    pub command: String,
    pub bell: bool,
    pub on_new_mr: bool,
    pub on_approval: bool,
    pub on_new_discussion: bool,
    pub on_pipeline_change: bool,
    pub on_merged_or_closed: bool,
    pub only_when_unfocused: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: NotifyBackend::Auto,
            command: String::new(),
            bell: true,
            on_new_mr: true,
            on_approval: true,
            on_new_discussion: true,
            // Implemented, but off by default — these fire far more often than the
            // three above and would train the user to ignore notifications.
            on_pipeline_change: false,
            on_merged_or_closed: false,
            only_when_unfocused: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotifyBackend {
    Auto,
    Osc9,
    Osc777,
    Command,
    None,
}

// --------------------------------------------------------------------------- [browser]

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Browser {
    /// Empty means `open` on macOS and `xdg-open` on Linux.
    pub command: String,
}

// -------------------------------------------------------------------------- [[filter]]

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub name: String,
    pub scope: Scope,
    pub state: StateFilter,
    /// Per-filter override of `[ui].show_drafts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_drafts: Option<bool>,
    /// Per-filter override of `[ui].columns`, with the same `:wide` wide-only suffix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<ColumnSpec>>,
    /// Per-filter override of `[notifications].enabled`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,

    /// Required for `group` and `project` scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `group` scope only. `None` means "not set", which is what lets `validate` reject
    /// it on a scope where it has no meaning instead of silently ignoring it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_subgroups: Option<bool>,

    // The arguments below are only accepted by the group, project and instance query
    // roots; the `currentUser.*` connections do not take them. `validate` rejects the
    // combination rather than silently dropping the filter.
    pub labels: Vec<String>,
    pub not_labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// `true` for any reviewer, `false` for none; mutually exclusive with `reviewer`,
    /// since GitLab's `reviewerWildcardId` and `reviewerUsername` cannot both be set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_reviewer: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_after_days: Option<u32>,

    pub max_results: usize,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            name: String::new(),
            scope: Scope::Assigned,
            state: StateFilter::Opened,
            show_drafts: None,
            columns: None,
            notify: None,
            path: None,
            include_subgroups: None,
            labels: Vec::new(),
            not_labels: Vec::new(),
            author: None,
            assignee: None,
            reviewer: None,
            has_reviewer: None,
            milestone: None,
            target_branch: None,
            updated_after_days: None,
            max_results: 100,
        }
    }
}

impl Filter {
    pub fn named(name: &str, scope: Scope) -> Self {
        Self {
            name: name.into(),
            scope,
            ..Self::default()
        }
    }

    /// Whether subgroups are included, defaulting to true for a group scope.
    pub fn includes_subgroups(&self) -> bool {
        self.include_subgroups.unwrap_or(true)
    }

    /// Whether any argument that only the group/project roots accept has been set.
    ///
    /// `validate` uses this to produce one error naming every offending key rather than
    /// failing on the first and making the user re-run to find the next.
    pub fn root_only_args(&self) -> Vec<&'static str> {
        let mut set = Vec::new();
        if !self.labels.is_empty() {
            set.push("labels");
        }
        if !self.not_labels.is_empty() {
            set.push("not_labels");
        }
        for (present, key) in [
            (self.author.is_some(), "author"),
            (self.assignee.is_some(), "assignee"),
            (self.reviewer.is_some(), "reviewer"),
            (self.has_reviewer.is_some(), "has_reviewer"),
            (self.milestone.is_some(), "milestone"),
            (self.target_branch.is_some(), "target_branch"),
            (self.updated_after_days.is_some(), "updated_after_days"),
        ] {
            if present {
                set.push(key);
            }
        }
        set
    }

    /// A filesystem-safe identifier for this filter's cache file.
    ///
    /// Derived from the name, which `validate` guarantees is unique. Anything outside
    /// `[a-z0-9-]` becomes `-`, and the hash suffix keeps two names that differ only in
    /// punctuation from colliding onto one cache file.
    ///
    /// The stem falls back to `filter` when a name has no alphanumerics at all: an empty
    /// stem would leave the slug starting with `-`, and a file named `-1a2b3c4d` is read
    /// as a flag by anything that later globs the cache directory.
    pub fn slug(&self) -> String {
        use std::hash::{Hash, Hasher};

        let mut stem = String::with_capacity(self.name.len());
        for c in self.name.chars() {
            if c.is_ascii_alphanumeric() {
                stem.push(c.to_ascii_lowercase());
            } else if !stem.ends_with('-') {
                stem.push('-');
            }
        }
        let stem = stem.trim_matches('-');
        let stem = if stem.is_empty() { "filter" } else { stem };

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.name.hash(&mut hasher);
        format!("{stem}-{:08x}", hasher.finish() as u32)
    }
}

/// Which GraphQL root a filter queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Assigned,
    ReviewRequested,
    Authored,
    Group,
    Project,
    /// The unscoped `Query.mergeRequests` root: every merge request the token can see,
    /// instance-wide. Takes the same root-only arguments as `group`/`project`, since
    /// GitLab gives it the same argument set, but no `path`.
    Instance,
}

impl Scope {
    /// Whether this scope is rooted at `currentUser`, which is what decides which
    /// arguments a filter on it may set.
    pub const fn is_current_user(self) -> bool {
        matches!(
            self,
            Self::Assigned | Self::ReviewRequested | Self::Authored
        )
    }

    /// Whether this scope requires `path`.
    pub const fn requires_path(self) -> bool {
        matches!(self, Self::Group | Self::Project)
    }

    pub const fn key(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::ReviewRequested => "review_requested",
            Self::Authored => "authored",
            Self::Group => "group",
            Self::Project => "project",
            Self::Instance => "instance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StateFilter {
    Opened,
    Merged,
    Closed,
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typo must be rejected by the generated schema exactly like it is by serde: the
    /// whole point of shipping one is that an editor catches the mistake `deny_unknown_fields`
    /// would otherwise only catch at startup.
    #[test]
    fn the_schema_denies_unknown_fields_at_every_level() {
        let schema = json_schema();
        let value = serde_json::to_value(&schema).unwrap();

        assert_eq!(value["additionalProperties"], false, "root");
        for (name, def) in value["$defs"].as_object().unwrap() {
            if def["type"] == "object" {
                assert_eq!(def["additionalProperties"], false, "{name}");
            }
        }
    }

    /// The shipped default config, converted from TOML to JSON, must validate against the
    /// schema generated from the same types — otherwise the schema would reject the very
    /// file `mrq init-config` writes.
    #[test]
    fn the_schema_accepts_the_shipped_default_config() {
        let schema_json = serde_json::to_value(json_schema()).unwrap();
        let validator = jsonschema::validator_for(&schema_json).unwrap();

        let default_toml: toml::Value =
            toml::from_str(crate::config::init::DEFAULT_CONFIG).unwrap();
        let default_json = serde_json::to_value(default_toml).unwrap();

        let errors: Vec<_> = validator.iter_errors(&default_json).collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A key the schema does not know about must fail validation, the same way it fails
    /// `toml::from_str` — an editor and `mrq` itself must agree on what a typo is.
    #[test]
    fn the_schema_rejects_an_unknown_key() {
        let schema_json = serde_json::to_value(json_schema()).unwrap();
        let validator = jsonschema::validator_for(&schema_json).unwrap();

        let doc = serde_json::json!({"ui": {"show_draft": true}});
        assert!(!validator.is_valid(&doc));
    }

    fn schema_validator() -> jsonschema::Validator {
        jsonschema::validator_for(&serde_json::to_value(json_schema()).unwrap()).unwrap()
    }

    fn schema_accepts(doc: &str) -> bool {
        let toml: toml::Value = toml::from_str(doc).unwrap();
        schema_validator().is_valid(&serde_json::to_value(toml).unwrap())
    }

    /// TOML has no `null`, so the schema must never offer it as a type or a default.
    #[test]
    fn the_schema_never_mentions_null() {
        fn walk(v: &serde_json::Value, path: &str) {
            match v {
                serde_json::Value::Null => panic!("null at {path}"),
                serde_json::Value::String(s) if path.ends_with("/type") => {
                    assert_ne!(s, "null", "{path}");
                }
                serde_json::Value::Object(m) => {
                    m.iter().for_each(|(k, c)| walk(c, &format!("{path}/{k}")));
                }
                serde_json::Value::Array(a) => {
                    a.iter()
                        .enumerate()
                        .for_each(|(i, c)| walk(c, &format!("{path}/{i}")));
                }
                _ => {}
            }
        }
        walk(&serde_json::to_value(json_schema()).unwrap(), "");
    }

    /// The schema and `validate` must agree on every rule the schema expresses: each
    /// document `validate` rejects is rejected by the schema, each valid one accepted.
    #[test]
    fn the_schema_agrees_with_validate() {
        let rejected = [
            "[refresh]\ninterval_secs = 5\n",
            "[gitlab]\ntimeout_secs = 0\n",
            "[gitlab]\nmax_concurrent_requests = 0\n",
            "[gitlab]\nurl = \"gitlab.example.com\"\n",
            "[ui]\ncolumns = []\n",
            "[ui]\ncolumns = [\"title:wide\"]\n",
            "[[filter]]\nname = \"\"\n",
            "[[filter]]\nname = \"a\"\nscope = \"group\"\n",
            "[[filter]]\nname = \"a\"\nscope = \"group\"\npath = \"/x\"\n",
            "[[filter]]\nname = \"a\"\npath = \"x\"\n",
            "[[filter]]\nname = \"a\"\nscope = \"project\"\npath = \"x\"\ninclude_subgroups = true\n",
            "[[filter]]\nname = \"a\"\nscope = \"group\"\npath = \"x\"\nreviewer = \"u\"\nhas_reviewer = true\n",
            "[[filter]]\nname = \"a\"\nlabels = [\"x\"]\n",
            "[[filter]]\nname = \"a\"\nscope = \"authored\"\nmilestone = \"m\"\n",
            "[notifications]\nbackend = \"command\"\n",
            "[notifications]\nbackend = \"command\"\ncommand = \"  \"\n",
        ];
        for doc in rejected {
            assert!(!schema_accepts(doc), "schema should reject:\n{doc}");
            let mut config: Config = toml::from_str(doc).unwrap();
            assert!(
                crate::config::validate::validate(&mut config).is_err(),
                "validate should reject:\n{doc}"
            );
        }

        // Checked by the theme and keymap loaders rather than `validate`.
        for doc in [
            "[skin.colors]\nnope = \"#ffffff\"\n",
            "[skin.colors]\nred = \"red\"\n",
            "[keys]\nnope = [\"x\"]\n",
        ] {
            assert!(!schema_accepts(doc), "schema should reject:\n{doc}");
        }

        let accepted = [
            "",
            "[refresh]\ninterval_secs = 30\n",
            "[ui]\ncolumns = [\"title\", \"id:wide\", \"updated\"]\n",
            "[[filter]]\nname = \"a\"\nscope = \"group\"\npath = \"acme/x\"\ninclude_subgroups = true\nlabels = [\"l\"]\n",
            "[[filter]]\nname = \"a\"\nscope = \"instance\"\nlabels = [\"l\"]\nhas_reviewer = false\n",
            "[[filter]]\nname = \"a\"\nscope = \"project\"\npath = \"acme/x\"\nauthor = \"u\"\n",
            "[notifications]\nbackend = \"command\"\ncommand = \"notify {title}\"\n",
            "[skin]\nname = \"Nord\"\n[skin.colors]\nred = \"#ff0000\"\n",
            "[keys]\nquit = [\"q\"]\n",
        ];
        for doc in accepted {
            assert!(schema_accepts(doc), "schema should accept:\n{doc}");
            let mut config: Config = toml::from_str(doc).unwrap();
            crate::config::validate::validate(&mut config)
                .unwrap_or_else(|e| panic!("validate should accept:\n{doc}\n{e}"));
        }
    }

    #[test]
    fn empty_document_yields_the_built_in_defaults() {
        let parsed: Config = toml::from_str("").unwrap();
        assert_eq!(parsed, Config::default());
    }

    /// These are the values a user gets with no config file at all.
    #[test]
    fn documented_defaults() {
        let c = Config::default();
        assert_eq!(c.gitlab.url, "https://gitlab.com");
        assert_eq!(c.gitlab.timeout_secs, 20);
        assert_eq!(c.gitlab.max_concurrent_requests, 4);
        assert_eq!(c.refresh.interval_secs, 300, "5 minutes");
        assert_eq!(c.refresh.jitter_secs, 15);
        assert!(c.refresh.refresh_on_focus);
        assert!(!c.refresh.pause_when_unfocused);
        assert!(!c.ui.ascii, "Unicode glyphs by default");
        assert_eq!(c.skin.name, "catppuccin-mocha");
        assert!(c.skin.colors.is_empty());
        assert!(!c.ui.show_drafts, "drafts are hidden by default");
        assert!(!c.ui.mouse, "mouse off, so native selection keeps working");
        assert_eq!(c.ui.columns, ColumnSpec::DEFAULT.to_vec());
        let wide: Vec<_> =
            c.ui.columns
                .iter()
                .filter(|s| s.wide)
                .map(|s| s.column)
                .collect();
        assert_eq!(
            wide,
            [Column::Id, Column::Approver, Column::Reviewer, Column::Diff]
        );
        assert!(!c.ui.wide, "wide mode starts off");
        assert_eq!(c.ui.assigned_display, PeopleDisplay::YesNo);
        assert_eq!(c.ui.approver_display, PeopleDisplay::Username);
        assert_eq!(c.ui.reviewer_display, PeopleDisplay::Username);
        assert_eq!(c.ui.link, LinkTarget::Title);
        assert_eq!(c.sort.column, Column::Updated);
        assert_eq!(c.sort.order, Order::Desc);
        assert!(c.sort.drafts_last);
        assert!(c.notifications.on_new_mr);
        assert!(!c.notifications.on_pipeline_change, "noisy, off by default");
        assert!(c.notifications.only_when_unfocused);
    }

    /// With no configuration, the list shows the MRs assigned to me.
    #[test]
    fn default_filter_is_assigned_to_me() {
        let c = Config::default();
        assert_eq!(c.filters.len(), 1);
        assert_eq!(c.filters[0].scope, Scope::Assigned);
        assert_eq!(c.filters[0].state, StateFilter::Opened);
        assert_eq!(c.filters[0].show_drafts, None, "inherits [ui].show_drafts");
        assert_eq!(
            c.filters[0].notify, None,
            "inherits [notifications].enabled"
        );
    }

    /// The whole point of strict parsing: a typo must not be silently ignored,
    /// because the user cannot distinguish that from mrq being broken.
    #[test]
    fn unknown_keys_are_rejected() {
        let cases = [
            ("[ui]\nshow_draft = true\n", "show_draft"),
            ("[gitlab]\nurl2 = \"x\"\n", "url2"),
            ("[refresh]\ninterval = 60\n", "interval"),
            ("[[filter]]\nname = \"a\"\nlable = \"x\"\n", "lable"),
            ("[nope]\nx = 1\n", "nope"),
        ];
        for (doc, key) in cases {
            let err = toml::from_str::<Config>(doc).unwrap_err();
            assert!(
                err.to_string().contains(key),
                "error for `{key}` should name it, got: {err}"
            );
        }
    }

    #[test]
    fn unknown_enum_values_are_rejected_and_list_the_alternatives() {
        let err = toml::from_str::<Config>("[sort]\norder = \"sideways\"\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sideways"), "{msg}");
        assert!(msg.contains("desc"), "should list the valid values: {msg}");
    }

    #[test]
    fn the_skin_table_parses_a_name_and_per_swatch_overrides() {
        let c: Config =
            toml::from_str("[skin]\nname = \"nord\"\n\n[skin.colors]\nred = \"#ff0000\"\n")
                .unwrap();

        assert_eq!(c.skin.name, "nord");
        assert_eq!(c.skin.colors["red"], "#ff0000");
    }

    #[test]
    fn partial_tables_keep_the_other_defaults() {
        let c: Config = toml::from_str("[refresh]\ninterval_secs = 60\n").unwrap();
        assert_eq!(c.refresh.interval_secs, 60);
        assert_eq!(c.refresh.jitter_secs, 15, "untouched keys keep defaults");
        assert_eq!(c.gitlab.url, "https://gitlab.com");
    }

    #[test]
    fn the_spec_example_filters_parse() {
        let doc = r#"
[[filter]]
name = "Assigned"
scope = "assigned"
state = "opened"
show_drafts = false

[[filter]]
name = "Reviewing"
scope = "review_requested"

[[filter]]
name = "Platform"
scope = "group"
path = "acme/platform"
include_subgroups = true
labels = ["team::platform"]
not_labels = ["wip"]
author = "someuser"
assignee = "someuser"
reviewer = "someuser"
milestone = "24.Q3"
target_branch = "main"
updated_after_days = 30
max_results = 100

[[filter]]
name = "SRE ask"
scope = "group"
path = "acme/platform"
labels = ["sre-review::ask"]
has_reviewer = false
notify = false
"#;
        let c: Config = toml::from_str(doc).unwrap();
        assert_eq!(c.filters.len(), 4);
        assert_eq!(c.filters[1].scope, Scope::ReviewRequested);
        assert_eq!(c.filters[1].max_results, 100, "default applies");
        assert_eq!(c.filters[1].notify, None, "default applies");
        assert_eq!(c.filters[3].notify, Some(false));

        let platform = &c.filters[2];
        assert_eq!(platform.scope, Scope::Group);
        assert_eq!(platform.path.as_deref(), Some("acme/platform"));
        assert_eq!(platform.labels, ["team::platform"]);
        assert_eq!(platform.updated_after_days, Some(30));

        assert_eq!(c.filters[3].has_reviewer, Some(false));
    }

    #[test]
    fn keys_table_is_captured_verbatim_for_the_keymap() {
        let c: Config =
            toml::from_str("[keys]\nquit = [\"q\", \"ctrl-c\"]\nrefresh = [\"ctrl-r\"]\n").unwrap();
        assert_eq!(c.keys["quit"], ["q", "ctrl-c"]);
        assert_eq!(c.keys["refresh"], ["ctrl-r"]);
        assert_eq!(c.keys.len(), 2, "only what the file named");
    }

    /// Scope drives the argument restriction, so the classification has to be right.
    #[test]
    fn scope_classification() {
        for s in [Scope::Assigned, Scope::ReviewRequested, Scope::Authored] {
            assert!(s.is_current_user(), "{s:?}");
            assert!(!s.requires_path(), "{s:?}");
        }
        for s in [Scope::Group, Scope::Project] {
            assert!(!s.is_current_user(), "{s:?}");
            assert!(s.requires_path(), "{s:?}");
        }
        assert!(!Scope::Instance.is_current_user());
        assert!(!Scope::Instance.requires_path());
    }

    #[test]
    fn root_only_args_reports_every_offender_at_once() {
        let f = Filter {
            labels: vec!["a".into()],
            author: Some("u".into()),
            milestone: Some("m".into()),
            ..Filter::named("X", Scope::Assigned)
        };
        assert_eq!(f.root_only_args(), ["labels", "author", "milestone"]);
        assert!(
            Filter::named("Y", Scope::Assigned)
                .root_only_args()
                .is_empty()
        );
    }

    /// Cache filenames come from the slug, so two differently-named filters must never
    /// share one.
    #[test]
    fn slugs_are_filesystem_safe_and_distinct() {
        let a = Filter::named("Team / Platform", Scope::Group).slug();
        let b = Filter::named("Team: Platform", Scope::Group).slug();

        assert_ne!(a, b, "punctuation-only differences must not collide");
        for slug in [&a, &b] {
            assert!(
                slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "unsafe characters in {slug}"
            );
            assert!(!slug.contains('/'), "{slug} would escape the cache dir");
        }
        assert_eq!(
            Filter::named("Assigned", Scope::Assigned).slug(),
            Filter::named("Assigned", Scope::Assigned).slug(),
            "stable across runs"
        );
    }

    /// A name made entirely of punctuation must still produce a usable filename rather
    /// than an empty one that would collide with the directory itself.
    #[test]
    fn slug_survives_a_name_with_no_alphanumerics() {
        let slug = Filter::named("///", Scope::Group).slug();
        assert!(!slug.is_empty());
        assert!(!slug.starts_with('-'), "{slug}");
        assert!(slug.chars().any(|c| c.is_ascii_alphanumeric()), "{slug}");
    }

    #[test]
    fn order_inverts() {
        assert_eq!(Order::Asc.inverted(), Order::Desc);
        assert_eq!(Order::Desc.inverted(), Order::Asc);
    }

    #[test]
    fn column_keys_round_trip_through_serde() {
        for col in Column::DEFAULT {
            let toml_doc = format!("[ui]\ncolumns = [\"{}\"]\n", col.key());
            let c: Config = toml::from_str(&toml_doc).unwrap();
            assert_eq!(
                c.ui.columns,
                vec![ColumnSpec::from(col)],
                "{} did not round-trip",
                col.key()
            );
        }
    }

    #[test]
    fn the_wide_suffix_parses_and_round_trips() {
        let c: Config = toml::from_str("[ui]\ncolumns = [\"title\", \"id:wide\"]\n").unwrap();
        assert_eq!(
            c.ui.columns,
            [
                ColumnSpec::from(Column::Title),
                ColumnSpec {
                    column: Column::Id,
                    wide: true
                }
            ]
        );
        let out = toml::to_string(&c).unwrap();
        assert!(out.contains("\"id:wide\""), "{out}");
        assert_eq!(toml::from_str::<Config>(&out).unwrap(), c);
    }

    #[test]
    fn a_bad_suffix_or_key_is_rejected_with_guidance() {
        let err = toml::from_str::<Config>("[ui]\ncolumns = [\"id:x\"]\n").unwrap_err();
        assert!(err.to_string().contains("only \":wide\""), "{err}");

        let err = toml::from_str::<Config>("[ui]\ncolumns = [\"nope:wide\"]\n").unwrap_err();
        assert!(
            err.to_string().contains("approved"),
            "lists valid keys: {err}"
        );
    }

    #[test]
    fn a_filter_can_override_the_columns() {
        let c: Config =
            toml::from_str("[[filter]]\nname = \"a\"\ncolumns = [\"title\", \"branch:wide\"]\n")
                .unwrap();
        let cols = c.filters[0].columns.as_ref().unwrap();
        assert_eq!(cols.len(), 2);
        assert!(cols[1].wide);
        assert_eq!(Config::default().filters[0].columns, None);
    }
}
