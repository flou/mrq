//! Key-spec grammar, the default bindings, and the merge that produces a keymap.
//!
//! Three things have to be true at once: the grammar must accept what the schema
//! writes, a user table must merge *over* the defaults rather than replacing them, and a
//! key claimed by two actions must be a startup error.
//!
//! # Shift normalisation
//!
//! `shift-S` and `S` are the same binding, and terminals disagree about whether an
//! uppercase character arrives with the `SHIFT` modifier set — it depends on whether the
//! kitty keyboard protocol is active. Both parsing and event lookup therefore run through
//! [`Key::normalise`], which folds `SHIFT` into the character itself for character keys
//! and leaves it alone for named keys, where `shift-tab` really is distinct from `tab`.
//! Without that, a binding would work in one terminal and silently not in another.

use std::collections::{BTreeMap, HashMap};
use std::fmt::{self, Write as _};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::error::ConfigError;

/// Everything the user can do, as named in the `[keys]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    Quit,
    Refresh,
    RefreshVisible,
    Down,
    Up,
    PageDown,
    PageUp,
    Top,
    Bottom,
    OpenMr,
    OpenPipeline,
    OpenProject,
    CopyUrl,
    CopyBranch,
    ShowDetails,
    ToggleDrafts,
    ToggleWide,
    SortMenu,
    InvertSort,
    SkinMenu,
    NextFilter,
    PrevFilter,
    FilterMenu,
    Search,
    ClearSearch,
    Help,
    LogMenu,
}

/// Grouping for the help popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    Navigation,
    Actions,
    View,
    Filters,
    Application,
}

impl Category {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Navigation => "Navigation",
            Self::Actions => "Actions",
            Self::View => "View",
            Self::Filters => "Filters",
            Self::Application => "Application",
        }
    }

    pub const ALL: [Self; 5] = [
        Self::Navigation,
        Self::Actions,
        Self::View,
        Self::Filters,
        Self::Application,
    ];
}

impl Action {
    /// Every action, in help-popup order.
    pub const ALL: [Self; 27] = [
        Self::Down,
        Self::Up,
        Self::PageDown,
        Self::PageUp,
        Self::Top,
        Self::Bottom,
        Self::OpenMr,
        Self::OpenPipeline,
        Self::OpenProject,
        Self::CopyUrl,
        Self::CopyBranch,
        Self::ShowDetails,
        Self::ToggleDrafts,
        Self::ToggleWide,
        Self::SortMenu,
        Self::InvertSort,
        Self::SkinMenu,
        Self::NextFilter,
        Self::PrevFilter,
        Self::FilterMenu,
        Self::Search,
        Self::ClearSearch,
        Self::Refresh,
        Self::RefreshVisible,
        Self::Help,
        Self::LogMenu,
        Self::Quit,
    ];

    /// The `[keys]` table name for this action.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Refresh => "refresh",
            Self::RefreshVisible => "refresh_visible",
            Self::Down => "down",
            Self::Up => "up",
            Self::PageDown => "page_down",
            Self::PageUp => "page_up",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::OpenMr => "open_mr",
            Self::OpenPipeline => "open_pipeline",
            Self::OpenProject => "open_project",
            Self::CopyUrl => "copy_url",
            Self::CopyBranch => "copy_branch",
            Self::ShowDetails => "show_details",
            Self::ToggleDrafts => "toggle_drafts",
            Self::ToggleWide => "toggle_wide",
            Self::SortMenu => "sort_menu",
            Self::InvertSort => "invert_sort",
            Self::SkinMenu => "skin_menu",
            Self::NextFilter => "next_filter",
            Self::PrevFilter => "prev_filter",
            Self::FilterMenu => "filter_menu",
            Self::Search => "search",
            Self::ClearSearch => "clear_search",
            Self::Help => "help",
            Self::LogMenu => "log_menu",
        }
    }

    fn from_key(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.key() == name)
    }

    /// One-line description, shown in the help popup.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Quit => "Quit",
            Self::Refresh => "Refresh every filter now",
            Self::RefreshVisible => "Refresh only the current filter",
            Self::Down => "Move down",
            Self::Up => "Move up",
            Self::PageDown => "Half page down",
            Self::PageUp => "Half page up",
            Self::Top => "Jump to first row",
            Self::Bottom => "Jump to last row",
            Self::OpenMr => "Open the merge request in the browser",
            Self::OpenPipeline => "Open the latest pipeline",
            Self::OpenProject => "Open the project",
            Self::CopyUrl => "Copy the merge request URL",
            Self::CopyBranch => "Copy the source branch name",
            Self::ShowDetails => "Show merge request details",
            Self::ToggleDrafts => "Show or hide drafts",
            Self::ToggleWide => "Show or hide wide-only columns",
            Self::SortMenu => "Choose the sort column",
            Self::InvertSort => "Invert the sort order",
            Self::SkinMenu => "Change the colour skin",
            Self::NextFilter => "Next filter",
            Self::PrevFilter => "Previous filter",
            Self::FilterMenu => "Switch filter",
            Self::Search => "Search",
            Self::ClearSearch => "Clear search",
            Self::Help => "Show this help",
            Self::LogMenu => "Show recent log lines",
        }
    }

    pub const fn category(self) -> Category {
        match self {
            Self::Down | Self::Up | Self::PageDown | Self::PageUp | Self::Top | Self::Bottom => {
                Category::Navigation
            }
            Self::OpenMr
            | Self::OpenPipeline
            | Self::OpenProject
            | Self::CopyUrl
            | Self::CopyBranch
            | Self::ShowDetails => Category::Actions,
            Self::ToggleDrafts
            | Self::ToggleWide
            | Self::SortMenu
            | Self::InvertSort
            | Self::SkinMenu => Category::View,
            Self::NextFilter
            | Self::PrevFilter
            | Self::FilterMenu
            | Self::Search
            | Self::ClearSearch => Category::Filters,
            Self::Quit | Self::Refresh | Self::RefreshVisible | Self::Help | Self::LogMenu => {
                Category::Application
            }
        }
    }
}

/// A normalised key binding: a crossterm code plus the modifiers that survive
/// normalisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Key {
    /// Fold `SHIFT` into the character for character keys.
    ///
    /// An uppercase character already encodes the shift; whether the terminal *also*
    /// reports the modifier depends on the kitty keyboard protocol being active, so
    /// keeping it would make a binding terminal-dependent.
    const fn normalise(code: KeyCode, mods: KeyModifiers) -> Self {
        match code {
            KeyCode::Char(c) if mods.contains(KeyModifiers::SHIFT) => {
                let upper = c.to_ascii_uppercase();
                Self {
                    code: KeyCode::Char(upper),
                    mods: mods.difference(KeyModifiers::SHIFT),
                }
            }
            // Legacy terminals report shift-tab as its own keycode (CSI Z) rather than as
            // `Tab` with `SHIFT` set, which is what the kitty protocol sends and what
            // `parse_key("shift-tab")` produces. One spec has to match both.
            KeyCode::BackTab => Self {
                code: KeyCode::Tab,
                mods: mods.union(KeyModifiers::SHIFT),
            },
            _ => Self { code, mods },
        }
    }

    /// The lookup key for an incoming terminal event.
    ///
    /// Returns `None` for key *releases* and repeats, which the kitty protocol reports
    /// and which would otherwise fire every action twice.
    pub fn from_event(event: KeyEvent) -> Option<Self> {
        if event.kind != KeyEventKind::Press {
            return None;
        }
        Some(Self::normalise(event.code, event.modifiers))
    }

    /// Render back to the config grammar, for the help popup.
    pub fn to_spec(self) -> String {
        let mut out = String::new();
        for (flag, name) in [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SUPER, "super"),
            (KeyModifiers::SHIFT, "shift"),
        ] {
            if self.mods.contains(flag) {
                out.push_str(name);
                out.push('-');
            }
        }
        match self.code {
            KeyCode::Char(' ') => out.push_str("space"),
            KeyCode::Char(c) => out.push(c),
            // A `String` write never fails; nothing to propagate.
            KeyCode::F(n) => _ = write!(out, "f{n}"),
            other => out.push_str(named_key_name(other).unwrap_or("?")),
        }
        out
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_spec())
    }
}

/// The named keys the `[keys]` table accepts.
fn named_key(name: &str) -> Option<KeyCode> {
    Some(match name {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pagedown" => KeyCode::PageDown,
        "pageup" => KeyCode::PageUp,
        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        _ => return None,
    })
}

const fn named_key_name(code: KeyCode) -> Option<&'static str> {
    Some(match code {
        KeyCode::Enter => "enter",
        KeyCode::Esc => "esc",
        KeyCode::Tab | KeyCode::BackTab => "tab",
        KeyCode::Backspace => "backspace",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageDown => "pagedown",
        KeyCode::PageUp => "pageup",
        KeyCode::Insert => "insert",
        KeyCode::Delete => "delete",
        _ => return None,
    })
}

/// Parse one key specification: `[modifier-]*key`.
pub fn parse_key(spec: &str) -> Result<Key, ConfigError> {
    let invalid = |reason: &str| ConfigError::KeySpec {
        spec: spec.to_owned(),
        reason: reason.to_owned(),
    };

    if spec.is_empty() {
        return Err(invalid("empty key specification"));
    }

    // '-' is both the separator and a bindable key, so a spec ending in '-' means the
    // minus key: `-` on its own, and `ctrl--` for ctrl plus minus. Splitting naively
    // leaves an empty final part in those cases, which is the minus key.
    let mut parts: Vec<&str> = spec.split('-').collect();
    #[expect(clippy::expect_used, reason = "str::split never yields zero parts")]
    let (last, modifier_parts) = parts.split_last_mut().expect("split yields one part");
    if last.is_empty() {
        *last = "-";
    }
    // The rewrite above can leave an empty modifier slot (`-` splits to two empties);
    // drop those rather than reporting an "unknown modifier ``".
    let modifier_parts: Vec<&str> = modifier_parts
        .iter()
        .copied()
        .filter(|p| !p.is_empty())
        .collect();
    let last: &str = last;

    let mut mods = KeyModifiers::empty();
    for part in &modifier_parts {
        let flag = match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "option" | "meta" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "super" | "cmd" | "command" => KeyModifiers::SUPER,
            other => {
                return Err(invalid(&format!(
                    "unknown modifier `{other}`; expected ctrl, alt, shift or super"
                )));
            }
        };
        mods |= flag;
    }

    let lower = last.to_ascii_lowercase();
    let code = if let Some(code) = named_key(&lower) {
        code
    } else if let Some(rest) = lower.strip_prefix('f')
        && let Ok(n) = rest.parse::<u8>()
        && (1..=12).contains(&n)
    {
        KeyCode::F(n)
    } else {
        let mut chars = last.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => KeyCode::Char(c),
            _ => {
                return Err(invalid(&format!(
                    "`{last}` is not a single character or a known key name"
                )));
            }
        }
    };

    Ok(Key::normalise(code, mods))
}

/// A resolved keymap, queryable in both directions.
///
/// Both directions are needed and must not drift apart: dispatch looks up key to action,
/// and the help popup enumerates action to keys. Help is generated from the resolved
/// map so it cannot lie to a user who has rebound something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    by_key: HashMap<Key, Action>,
    by_action: BTreeMap<Action, Vec<Key>>,
}

impl Keymap {
    /// The action bound to an incoming terminal event, if any.
    pub fn action_for(&self, event: KeyEvent) -> Option<Action> {
        let key = Key::from_event(event)?;
        self.by_key.get(&key).copied()
    }

    /// The keys bound to an action, in configured order.
    pub fn keys_for(&self, action: Action) -> &[Key] {
        self.by_action.get(&action).map_or(&[], Vec::as_slice)
    }

    /// Actions in a help category that currently have at least one binding.
    pub fn bound_in(&self, category: Category) -> Vec<Action> {
        Action::ALL
            .into_iter()
            .filter(|a| a.category() == category && !self.keys_for(*a).is_empty())
            .collect()
    }

    fn from_pairs(pairs: BTreeMap<Action, Vec<Key>>) -> Result<Self, ConfigError> {
        let mut by_key: HashMap<Key, Action> = HashMap::new();

        // Iterate in action order so the reported "first" binding is deterministic;
        // a conflict message that changes between runs is not actionable.
        for (action, keys) in &pairs {
            for key in keys {
                if let Some(existing) = by_key.get(key) {
                    return Err(ConfigError::DuplicateKeybind {
                        key: key.to_spec(),
                        first: existing.key().to_owned(),
                        second: action.key().to_owned(),
                    });
                }
                by_key.insert(*key, *action);
            }
        }

        Ok(Self {
            by_key,
            by_action: pairs,
        })
    }
}

/// The default bindings, exactly as shipped in the `[keys]` block.
///
/// `refresh_visible` and `log_menu` are deliberately unbound: reserved actions with no
/// default key.
pub const DEFAULT_BINDINGS: &[(Action, &[&str])] = &[
    (Action::Quit, &["q", "ctrl-c"]),
    (Action::Refresh, &["ctrl-r"]),
    (Action::RefreshVisible, &[]),
    (Action::Down, &["j", "down"]),
    (Action::Up, &["k", "up"]),
    (Action::PageDown, &["ctrl-d", "pagedown"]),
    (Action::PageUp, &["ctrl-u", "pageup"]),
    (Action::Top, &["g", "home"]),
    (Action::Bottom, &["shift-G", "end"]),
    (Action::OpenMr, &["o", "enter"]),
    (Action::OpenPipeline, &["p"]),
    (Action::OpenProject, &["shift-O"]),
    (Action::CopyUrl, &["y"]),
    (Action::CopyBranch, &["shift-Y"]),
    (Action::ShowDetails, &["i"]),
    (Action::ToggleDrafts, &["d"]),
    (Action::ToggleWide, &["w"]),
    (Action::SortMenu, &["shift-S"]),
    (Action::InvertSort, &["shift-I"]),
    (Action::SkinMenu, &["ctrl-t"]),
    (Action::NextFilter, &["]", "right"]),
    (Action::PrevFilter, &["[", "left"]),
    (Action::FilterMenu, &["f"]),
    (Action::Search, &["/"]),
    (Action::ClearSearch, &["esc"]),
    (Action::Help, &["?", "f1"]),
    (Action::LogMenu, &[]),
];

/// Build the keymap from the defaults merged with a user `[keys]` table.
///
/// The table merges *over* the defaults, so a partial table only overrides
/// what it names, and `action = []` unbinds.
pub fn resolve(user: &BTreeMap<String, Vec<String>>) -> Result<Keymap, ConfigError> {
    let mut pairs: BTreeMap<Action, Vec<Key>> = BTreeMap::new();

    for (action, specs) in DEFAULT_BINDINGS {
        #[expect(
            clippy::expect_used,
            reason = "covered by every test that calls resolve(&BTreeMap::new())"
        )]
        let keys = specs
            .iter()
            .map(|s| parse_key(s))
            .collect::<Result<Vec<_>, _>>()
            .expect("the shipped defaults must parse");
        pairs.insert(*action, keys);
    }

    for (name, specs) in user {
        let action = Action::from_key(name).ok_or_else(|| ConfigError::Invalid {
            key: format!("keys.{name}"),
            message: format!(
                "unknown action; expected one of: {}",
                Action::ALL
                    .iter()
                    .map(|a| a.key())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;

        let keys = specs
            .iter()
            .map(|s| parse_key(s))
            .collect::<Result<Vec<_>, _>>()?;

        // Replaces rather than appends: otherwise a user could never *remove* a default
        // binding, only add to it, and `action = []` would be meaningless.
        pairs.insert(action, keys);
    }

    Keymap::from_pairs(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec: &str) -> Key {
        parse_key(spec).unwrap_or_else(|e| panic!("`{spec}` should parse: {e}"))
    }

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn user(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_owned(),
                    v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    #[test]
    fn plain_characters_and_named_keys_parse() {
        assert_eq!(
            key("q"),
            Key::normalise(KeyCode::Char('q'), KeyModifiers::empty())
        );
        assert_eq!(
            key("/"),
            Key::normalise(KeyCode::Char('/'), KeyModifiers::empty())
        );
        assert_eq!(key("enter").code, KeyCode::Enter);
        assert_eq!(key("pageup").code, KeyCode::PageUp);
        assert_eq!(key("space").code, KeyCode::Char(' '));
        assert_eq!(key("f1").code, KeyCode::F(1));
        assert_eq!(key("f12").code, KeyCode::F(12));
    }

    #[test]
    fn modifiers_parse_and_are_case_insensitive() {
        assert_eq!(key("ctrl-r").mods, KeyModifiers::CONTROL);
        assert_eq!(key("CTRL-r"), key("ctrl-r"));
        assert_eq!(key("Ctrl-R"), key("ctrl-shift-r"));
        assert_eq!(key("alt-x").mods, KeyModifiers::ALT);
        assert_eq!(key("super-x").mods, KeyModifiers::SUPER);
    }

    /// `shift-S` and `S` are the same binding.
    #[test]
    fn shift_is_folded_into_the_character() {
        assert_eq!(key("shift-S"), key("S"));
        assert_eq!(key("shift-s"), key("S"));
        assert_eq!(key("shift-g"), key("G"));
        assert_ne!(key("S"), key("s"), "case still distinguishes bindings");
    }

    /// But not for named keys, where shift-tab is genuinely a different key.
    #[test]
    fn shift_is_preserved_for_named_keys() {
        assert_ne!(key("shift-tab"), key("tab"));
        assert_eq!(key("shift-tab").mods, KeyModifiers::SHIFT);
    }

    /// Legacy terminals send `BackTab`, not `Tab` + `SHIFT`, for shift-tab (CSI Z). The
    /// default `prev_filter` binding is `shift-tab`, and this is the event shape most
    /// terminals actually deliver for it — only the kitty protocol sends `Tab` + `SHIFT`.
    #[test]
    fn back_tab_events_match_the_shift_tab_spec() {
        assert_eq!(
            Key::from_event(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(key("shift-tab")),
        );

        let map = resolve(&BTreeMap::new()).unwrap();
        assert_eq!(
            map.action_for(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            None,
            "shift-tab is no longer bound by default"
        );
    }

    /// The reason normalisation exists: whether an uppercase character arrives with the
    /// SHIFT modifier depends on the kitty keyboard protocol being active, so both forms
    /// must resolve to the same binding.
    #[test]
    fn uppercase_events_match_with_or_without_the_shift_modifier() {
        let map = resolve(&BTreeMap::new()).unwrap();

        let with_shift = press(KeyCode::Char('S'), KeyModifiers::SHIFT);
        let without = press(KeyCode::Char('S'), KeyModifiers::empty());
        let lower_with_shift = press(KeyCode::Char('s'), KeyModifiers::SHIFT);

        assert_eq!(map.action_for(with_shift), Some(Action::SortMenu));
        assert_eq!(map.action_for(without), Some(Action::SortMenu));
        assert_eq!(map.action_for(lower_with_shift), Some(Action::SortMenu));
        assert_eq!(
            map.action_for(press(KeyCode::Char('s'), KeyModifiers::empty())),
            None,
            "and bare lowercase is not the same binding"
        );
    }

    /// The kitty protocol reports releases and repeats; acting on them fires everything
    /// twice.
    #[test]
    fn only_key_presses_dispatch() {
        let map = resolve(&BTreeMap::new()).unwrap();
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        release.kind = KeyEventKind::Release;

        assert_eq!(map.action_for(release), None);
        assert_eq!(
            map.action_for(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty())),
            Some(Action::Quit)
        );
    }

    #[test]
    fn bad_specs_are_rejected_with_a_reason() {
        for (spec, needle) in [
            ("", "empty"),
            ("hyper-x", "unknown modifier"),
            ("ctrl-nope", "not a single character"),
            ("f13", "not a single character"),
            ("f0", "not a single character"),
        ] {
            let err = parse_key(spec).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(needle), "`{spec}` -> {msg}");
        }
    }

    #[test]
    fn a_bare_minus_is_usable_as_a_key() {
        assert_eq!(key("-").code, KeyCode::Char('-'));
        assert_eq!(key("ctrl--").code, KeyCode::Char('-'));
        assert_eq!(key("ctrl--").mods, KeyModifiers::CONTROL);
    }

    /// The shipped default config's `[keys]` table; the defaults must match it exactly.
    #[test]
    fn defaults_match_the_spec_table() {
        let map = resolve(&BTreeMap::new()).unwrap();
        let expected = [
            ("q", Action::Quit),
            ("ctrl-c", Action::Quit),
            ("ctrl-r", Action::Refresh),
            ("j", Action::Down),
            ("down", Action::Down),
            ("k", Action::Up),
            ("up", Action::Up),
            ("ctrl-d", Action::PageDown),
            ("pagedown", Action::PageDown),
            ("ctrl-u", Action::PageUp),
            ("pageup", Action::PageUp),
            ("g", Action::Top),
            ("home", Action::Top),
            ("shift-g", Action::Bottom),
            ("end", Action::Bottom),
            ("o", Action::OpenMr),
            ("enter", Action::OpenMr),
            ("p", Action::OpenPipeline),
            ("shift-O", Action::OpenProject),
            ("y", Action::CopyUrl),
            ("shift-Y", Action::CopyBranch),
            ("i", Action::ShowDetails),
            ("d", Action::ToggleDrafts),
            ("w", Action::ToggleWide),
            ("shift-S", Action::SortMenu),
            ("shift-I", Action::InvertSort),
            ("ctrl-t", Action::SkinMenu),
            ("]", Action::NextFilter),
            ("right", Action::NextFilter),
            ("[", Action::PrevFilter),
            ("left", Action::PrevFilter),
            ("f", Action::FilterMenu),
            ("/", Action::Search),
            ("esc", Action::ClearSearch),
            ("?", Action::Help),
            ("f1", Action::Help),
        ];
        for (spec, action) in expected {
            let k = key(spec);
            assert_eq!(
                map.by_key.get(&k).copied(),
                Some(action),
                "`{spec}` should be bound to {}",
                action.key()
            );
        }
    }

    /// A shipped default that conflicts with another would make mrq refuse to start with
    /// no config file at all.
    #[test]
    fn the_shipped_defaults_have_no_conflicts() {
        assert!(resolve(&BTreeMap::new()).is_ok());
    }

    #[test]
    fn reserved_actions_ship_unbound_but_are_still_bindable() {
        let map = resolve(&BTreeMap::new()).unwrap();
        assert!(map.keys_for(Action::RefreshVisible).is_empty());
        assert!(map.keys_for(Action::LogMenu).is_empty());

        let map = resolve(&user(&[("log_menu", &["e"])])).unwrap();
        assert_eq!(
            map.action_for(press(KeyCode::Char('e'), KeyModifiers::empty())),
            Some(Action::LogMenu)
        );
    }

    /// A partial table only overrides what it names.
    #[test]
    fn a_user_table_merges_over_the_defaults() {
        let map = resolve(&user(&[("quit", &["ctrl-q"])])).unwrap();

        assert_eq!(
            map.action_for(press(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            map.action_for(press(KeyCode::Char('q'), KeyModifiers::empty())),
            None,
            "rebinding replaces the default rather than adding to it"
        );
        assert_eq!(
            map.action_for(press(KeyCode::Char('j'), KeyModifiers::empty())),
            Some(Action::Down),
            "untouched actions keep their defaults"
        );
    }

    #[test]
    fn an_empty_list_unbinds() {
        let map = resolve(&user(&[("quit", &[])])).unwrap();
        assert!(map.keys_for(Action::Quit).is_empty());
        assert_eq!(
            map.action_for(press(KeyCode::Char('q'), KeyModifiers::empty())),
            None
        );
    }

    /// A key claimed by two actions is fatal, and the message has to name
    /// both, or the user cannot tell what else needs rebinding.
    #[test]
    fn a_duplicate_binding_is_fatal_and_names_both_actions() {
        // `o` is a default for open_mr; binding it to toggle_drafts collides.
        let err = resolve(&user(&[("toggle_drafts", &["o"])])).unwrap_err();

        match &err {
            ConfigError::DuplicateKeybind { key, first, second } => {
                assert_eq!(key, "o");
                let both = [first.as_str(), second.as_str()];
                assert!(both.contains(&"open_mr"), "{both:?}");
                assert!(both.contains(&"toggle_drafts"), "{both:?}");
            }
            other => panic!("expected DuplicateKeybind, got {other:?}"),
        }
    }

    #[test]
    fn a_conflict_within_one_user_action_is_also_caught() {
        let err = resolve(&user(&[("quit", &["ctrl-q"]), ("help", &["ctrl-q"])])).unwrap_err();
        assert!(
            matches!(err, ConfigError::DuplicateKeybind { .. }),
            "{err:?}"
        );
    }

    /// Rebinding both sides of a collision must succeed — otherwise the conflict rule
    /// would make swapping two keys impossible.
    #[test]
    fn swapping_two_bindings_is_allowed() {
        let map = resolve(&user(&[
            ("toggle_drafts", &["o"]),
            ("open_mr", &["d", "enter"]),
        ]))
        .unwrap();

        assert_eq!(
            map.action_for(press(KeyCode::Char('o'), KeyModifiers::empty())),
            Some(Action::ToggleDrafts)
        );
        assert_eq!(
            map.action_for(press(KeyCode::Char('d'), KeyModifiers::empty())),
            Some(Action::OpenMr)
        );
    }

    #[test]
    fn an_unknown_action_name_is_rejected_and_lists_the_valid_ones() {
        let err = resolve(&user(&[("togle_drafts", &["x"])])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("togle_drafts"), "{msg}");
        assert!(msg.contains("toggle_drafts"), "should list valid: {msg}");
    }

    #[test]
    fn an_unparseable_user_spec_names_the_offending_value() {
        let err = resolve(&user(&[("quit", &["ctrl-nope"])])).unwrap_err();
        assert!(err.to_string().contains("ctrl-nope"), "{err}");
    }

    /// Help is generated from the resolved map, so it reflects rebinding.
    #[test]
    fn the_map_is_queryable_by_action_for_the_help_popup() {
        let map = resolve(&user(&[("quit", &["ctrl-q"])])).unwrap();

        let rendered: Vec<String> = map
            .keys_for(Action::Quit)
            .iter()
            .map(|k| k.to_spec())
            .collect();
        assert_eq!(rendered, ["ctrl-q"], "help shows the user's binding");

        let nav = map.bound_in(Category::Navigation);
        assert!(nav.contains(&Action::Down));
        assert!(!nav.contains(&Action::Quit));
        assert!(
            !map.bound_in(Category::Application)
                .contains(&Action::LogMenu),
            "unbound actions are not advertised in help"
        );
    }

    #[test]
    fn key_specs_round_trip_through_rendering() {
        for spec in [
            "q",
            "ctrl-c",
            "ctrl-r",
            "enter",
            "esc",
            "tab",
            "shift-tab",
            "pagedown",
            "f1",
            "/",
            "]",
            "space",
            "-",
        ] {
            let rendered = key(spec).to_spec();
            assert_eq!(
                key(&rendered),
                key(spec),
                "`{spec}` rendered as `{rendered}` and did not round-trip"
            );
        }
    }

    #[test]
    fn every_action_has_a_unique_config_name_and_a_description() {
        let mut names: Vec<&str> = Action::ALL.iter().map(|a| a.key()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "action config names must be unique");

        for action in Action::ALL {
            assert!(!action.description().is_empty(), "{}", action.key());
            assert!(Category::ALL.contains(&action.category()));
            assert_eq!(Action::from_key(action.key()), Some(action));
        }
    }

    #[test]
    fn every_action_has_a_default_entry() {
        for action in Action::ALL {
            assert!(
                DEFAULT_BINDINGS.iter().any(|(a, _)| *a == action),
                "{} is missing from DEFAULT_BINDINGS",
                action.key()
            );
        }
    }
}
