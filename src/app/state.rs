//! Per-filter tab state.
//!
//! Each configured filter is a tab owning its own snapshot, sort, draft
//! toggle, scroll, selection and search. Sharing any of those between tabs is the
//! obvious wrong implementation — switching tabs would silently carry your search across
//! — so the model makes it impossible by giving each tab its own.
//!
//! # Session-only state
//!
//! Sort order and the draft toggle live here and nowhere else: they are deliberately
//! not written back to the config file. Persisting them would mean a keystroke
//! silently editing a file the user hand-maintains.

use std::collections::HashSet;
use std::time::Instant;

use crate::config::schema::{Column, Filter, Order, Sort};
use crate::error::Error;
use crate::gitlab::fetch::Snapshot;
use crate::gitlab::model::MergeRequest;
use crate::gitlab::query::Fragment;

/// Where a tab's data currently stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchState {
    /// Nothing fetched yet this session.
    Idle,
    /// A request is in flight; the status bar shows a spinner.
    Fetching,
    /// The last fetch succeeded.
    Loaded,
    /// The last fetch failed. Any previous snapshot stays on screen.
    Failed { message: String },
}

impl FetchState {
    pub const fn is_fetching(&self) -> bool {
        matches!(self, Self::Fetching)
    }

    pub const fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// What, if anything, a tab needs the user to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    Healthy,
    /// The query had to give up fields to fit the instance's budget or tier. Every merge
    /// request is still listed.
    Degraded,
    /// The last fetch failed, or its response carried errors. The list may be wrong.
    Problem,
}

/// One configured filter's view state.
#[derive(Debug, Clone)]
pub struct Tab {
    /// Index into the configured filter list, which is also the 1..9 shortcut position.
    pub index: usize,
    pub name: String,

    /// The last successful result. Kept across failures so a transient error does not
    /// blank the table.
    merge_requests: Vec<MergeRequest>,
    pub fetched_at: Option<Instant>,
    /// When the scheduler will try again, for the status bar's countdown.
    pub next_refresh: Option<Instant>,
    pub state: FetchState,

    /// Set when the last response carried errors alongside data, so the tab can be
    /// marked without losing what did arrive.
    pub partial: bool,
    /// Set when `max_results` stopped the fetch before the server ran out. A capped list
    /// is a normal outcome the status bar explains — not, as it once was, the same flag
    /// as `partial`, which marks the tab as broken.
    pub truncated: bool,
    /// The query shape currently in use. A degraded one means fields are missing, which
    /// the UI has to be able to say.
    pub fragment: Fragment,

    pub sort_column: Column,
    pub sort_order: Order,
    /// Session-only: not written back to the config file.
    show_drafts: bool,

    pub search: Option<String>,
    pub scroll: usize,
    /// Tracked by id, not row index: a refresh that reorders rows must not move the
    /// cursor onto a different merge request.
    selected_id: Option<String>,
    /// Ids that were not in the previous snapshot, for the `*` gutter marker.
    ///
    /// Replaced wholesale by every fetch, so the markers always describe the latest
    /// refresh rather than accumulating across the session. A set, not a `Vec`: every
    /// visible row asks [`Self::is_new`] once per frame.
    new_ids: HashSet<String>,

    /// How old the cached rows were when they were loaded, when this tab was warm-started
    /// from the cache and no live fetch has landed yet.
    ///
    /// The age at load rather than a timestamp that keeps ticking: it is only shown in
    /// the window before the first live fetch replaces it, which is seconds, and a
    /// `Duration` avoids giving the status bar a second clock to reconcile.
    cached_age: Option<std::time::Duration>,
}

impl Tab {
    /// Build a tab from its configured filter and the global sort defaults.
    pub fn new(index: usize, filter: &Filter, sort: Sort, show_drafts: bool) -> Self {
        Self {
            index,
            name: filter.name.clone(),
            merge_requests: Vec::new(),
            fetched_at: None,
            next_refresh: None,
            state: FetchState::Idle,
            partial: false,
            truncated: false,
            fragment: Fragment::full(),
            sort_column: sort.column,
            sort_order: sort.order,
            // The per-filter override wins over the global default, which is what makes
            // a "Drafts" filter possible alongside a default that hides them.
            show_drafts: filter.show_drafts.unwrap_or(show_drafts),
            search: None,
            scroll: 0,
            selected_id: None,
            new_ids: HashSet::new(),
            cached_age: None,
        }
    }

    /// Every tab for a configuration, in config order.
    pub fn from_config(filters: &[Filter], sort: Sort, show_drafts: bool) -> Vec<Self> {
        filters
            .iter()
            .enumerate()
            .map(|(index, filter)| Self::new(index, filter, sort, show_drafts))
            .collect()
    }

    /// The merge requests as fetched, before filtering.
    pub fn all(&self) -> &[MergeRequest] {
        &self.merge_requests
    }

    pub const fn show_drafts(&self) -> bool {
        self.show_drafts
    }

    pub const fn toggle_drafts(&mut self) {
        self.show_drafts = !self.show_drafts;
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    pub fn select(&mut self, id: Option<String>) {
        self.selected_id = id;
    }

    /// Whether this tab's data is older than one refresh interval.
    ///
    /// A tab that has not fetched this session is stale whatever is on screen: warm-start
    /// rows are last session's by definition.
    pub fn is_stale(&self, now: Instant, interval: std::time::Duration) -> bool {
        match self.fetched_at {
            None => true,
            Some(at) => now.saturating_duration_since(at) >= interval,
        }
    }

    /// Whether a merge request arrived in the most recent refresh.
    pub fn is_new(&self, id: &str) -> bool {
        self.new_ids.contains(id)
    }

    /// Drop one row's `*`, because the user has moved the cursor onto it.
    ///
    /// Per row rather than wholesale: navigating to one arrival says nothing about the
    /// others, and clearing them all would lose the markers the user is about to scroll to.
    pub fn mark_seen(&mut self, id: &str) {
        self.new_ids.remove(id);
    }

    /// Drop every `*`, because the user asked for a refresh.
    ///
    /// Done at request time rather than when the snapshot lands, so the markers go even
    /// if the fetch fails: `ctrl-r` means "I am looking now", and leaving the previous
    /// cycle's arrivals marked over a failed refresh would claim they just arrived.
    pub fn clear_new(&mut self) {
        self.new_ids.clear();
    }

    pub const fn invert_sort(&mut self) {
        self.sort_order = self.sort_order.inverted();
    }

    /// Replace the snapshot after a successful fetch.
    ///
    /// Returns the ids that were not present before, which is what drives both the `*`
    /// markers and the notification rules.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot, at: Instant) -> Vec<String> {
        self.partial = snapshot.partial;
        self.truncated = snapshot.truncated;
        self.fragment = snapshot.fragment;
        self.apply_rows(snapshot.merge_requests, at)
    }

    /// How old the warm-started cached rows are, if this tab is showing them.
    ///
    /// `None` once a live fetch has landed, which is what stops the status bar saying
    /// "cached" over this session's own data.
    pub const fn cached_age(&self) -> Option<std::time::Duration> {
        self.cached_age
    }

    /// Put cached rows on screen for the first paint.
    ///
    /// Deliberately not `apply_rows`. Three things have to be different: the tab must not
    /// report itself `Loaded` or set `fetched_at`, because nothing has been fetched this
    /// session and the status bar and the diff both key on that; and nothing may be
    /// marked as newly arrived, because the warm render is silent.
    pub fn apply_cached(
        &mut self,
        merge_requests: Vec<MergeRequest>,
        age: std::time::Duration,
        truncated: bool,
        fragment: Fragment,
    ) {
        self.merge_requests = merge_requests;
        self.truncated = truncated;
        self.fragment = fragment;
        self.cached_age = Some(age);
        self.new_ids.clear();
    }

    /// The rows alone, for the cache warm start and for tests that only care about them.
    pub fn apply_rows(&mut self, merge_requests: Vec<MergeRequest>, at: Instant) -> Vec<String> {
        // A set, not a `Vec`: this scan is against the *previous* full snapshot, once per
        // incoming row, and `max_results` is user-configurable upward.
        let previous: HashSet<&str> = self.merge_requests.iter().map(|m| m.id.as_str()).collect();
        let arrived: Vec<String> = merge_requests
            .iter()
            .filter(|m| !previous.contains(m.id.as_str()))
            .map(|m| m.id.clone())
            .collect();

        // The first fetch of a session is not "new": everything would be, and the
        // resulting burst of markers carries no information.
        let first_fetch = self.fetched_at.is_none();

        self.merge_requests = merge_requests;
        self.fetched_at = Some(at);
        self.state = FetchState::Loaded;
        // This session's own data now, so the cache marker has to go.
        self.cached_age = None;
        self.new_ids = if first_fetch {
            HashSet::new()
        } else {
            arrived.iter().cloned().collect()
        };

        if first_fetch { Vec::new() } else { arrived }
    }

    /// Record a failed fetch. The previous snapshot is deliberately retained.
    pub fn apply_error(&mut self, error: &Error) {
        self.state = FetchState::Failed {
            message: error.to_string(),
        };
    }

    pub fn begin_fetch(&mut self) {
        self.state = FetchState::Fetching;
    }

    /// Whether the tab should be marked in the tab bar.
    #[cfg(test)]
    fn needs_attention(&self) -> bool {
        self.attention() != Attention::Healthy
    }

    /// Why the tab is marked, which is not the same question as whether it is.
    ///
    /// A degraded query still shows every merge request, just with fields missing; a
    /// failed or partial fetch means the list itself may be wrong. Giving them one
    /// marker would tell the user to go and look in both cases and let them find out
    /// which afterwards.
    pub fn attention(&self) -> Attention {
        if self.state.is_failed() || self.partial {
            Attention::Problem
        } else if self.fragment.is_degraded() {
            Attention::Degraded
        } else {
            Attention::Healthy
        }
    }
}

/// Every tab, plus which one is showing.
#[derive(Debug, Clone)]
pub struct Tabs {
    tabs: Vec<Tab>,
    active: usize,
}

impl Tabs {
    /// Build from configuration, optionally opening on a named filter (`--filter`).
    ///
    /// An unknown name is not an error: the filter list is user-configured and a typo in
    /// a one-off flag should not stop the program from starting on tab one.
    pub fn new(filters: &[Filter], sort: Sort, show_drafts: bool, initial: Option<&str>) -> Self {
        let tabs = Tab::from_config(filters, sort, show_drafts);
        let active = initial
            .and_then(|name| tabs.iter().position(|t| t.name == name))
            .unwrap_or(0);

        Self { tabs, active }
    }

    pub const fn len(&self) -> usize {
        self.tabs.len()
    }

    #[cfg(test)]
    pub const fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub const fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    pub fn get(&self, index: usize) -> Option<&Tab> {
        self.tabs.get(index)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Tab> {
        self.tabs.get_mut(index)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Tab> {
        self.tabs.iter_mut()
    }

    /// Switch to a tab by index, ignoring an out-of-range one.
    pub const fn activate(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.active = index;
            return true;
        }
        false
    }

    /// Switch by 1-based position, as the number keys address them.
    pub fn activate_position(&mut self, position: usize) -> bool {
        position
            .checked_sub(1)
            .is_some_and(|index| self.activate(index))
    }

    /// Cycle forward, wrapping.
    pub const fn next(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    /// Cycle backward, wrapping.
    pub const fn previous(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Scope, StateFilter};
    use crate::gitlab::model::fixtures::mr;

    fn filters(names: &[&str]) -> Vec<Filter> {
        names
            .iter()
            .map(|name| Filter::named(name, Scope::Assigned))
            .collect()
    }

    fn tabs(names: &[&str]) -> Tabs {
        Tabs::new(&filters(names), Sort::default(), false, None)
    }

    #[test]
    fn tabs_are_created_in_config_order() {
        let tabs = tabs(&["Assigned", "Reviewing", "Platform"]);

        assert_eq!(tabs.len(), 3);
        let names: Vec<&str> = tabs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Assigned", "Reviewing", "Platform"]);
        assert_eq!(tabs.get(1).unwrap().index, 1, "index matches position");
    }

    /// The number keys address tabs by config order.
    #[test]
    fn number_keys_select_by_position() {
        let mut tabs = tabs(&["One", "Two", "Three"]);

        assert!(tabs.activate_position(2));
        assert_eq!(tabs.active().unwrap().name, "Two");

        assert!(!tabs.activate_position(9), "out of range is ignored");
        assert_eq!(tabs.active().unwrap().name, "Two", "selection unchanged");

        assert!(!tabs.activate_position(0), "positions are 1-based");
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let mut tabs = tabs(&["One", "Two", "Three"]);

        tabs.next();
        tabs.next();
        assert_eq!(tabs.active_index(), 2);
        tabs.next();
        assert_eq!(tabs.active_index(), 0, "wraps forward");

        tabs.previous();
        assert_eq!(tabs.active_index(), 2, "wraps backward");
    }

    #[test]
    fn the_initial_filter_can_be_chosen_by_name() {
        let tabs = Tabs::new(
            &filters(&["One", "Two", "Three"]),
            Sort::default(),
            false,
            Some("Three"),
        );
        assert_eq!(tabs.active().unwrap().name, "Three");
    }

    /// A typo in a one-off flag should not stop the program from starting.
    #[test]
    fn an_unknown_initial_filter_falls_back_to_the_first_tab() {
        let tabs = Tabs::new(
            &filters(&["One", "Two"]),
            Sort::default(),
            false,
            Some("Nope"),
        );
        assert_eq!(tabs.active_index(), 0);
    }

    /// Sharing state between tabs would silently carry a search across a tab switch.
    #[test]
    fn tabs_do_not_share_state() {
        let mut tabs = tabs(&["One", "Two"]);

        tabs.get_mut(0).unwrap().search = Some("redis".into());
        tabs.get_mut(0).unwrap().toggle_drafts();
        tabs.get_mut(0).unwrap().scroll = 12;

        let second = tabs.get(1).unwrap();
        assert_eq!(second.search, None);
        assert!(!second.show_drafts());
        assert_eq!(second.scroll, 0);
    }

    /// The per-filter override is what makes a dedicated "Drafts" tab possible alongside
    /// a global default that hides them.
    #[test]
    fn a_per_filter_draft_override_beats_the_global_default() {
        let mut with_override = Filter::named("Drafts", Scope::Assigned);
        with_override.show_drafts = Some(true);
        let inheriting = Filter::named("Normal", Scope::Assigned);

        let tabs = Tabs::new(&[with_override, inheriting], Sort::default(), false, None);

        assert!(tabs.get(0).unwrap().show_drafts(), "override applies");
        assert!(!tabs.get(1).unwrap().show_drafts(), "default applies");
    }

    #[test]
    fn the_draft_toggle_is_per_tab_and_reversible() {
        let mut tabs = tabs(&["One", "Two"]);

        tabs.get_mut(0).unwrap().toggle_drafts();
        assert!(tabs.get(0).unwrap().show_drafts());
        assert!(!tabs.get(1).unwrap().show_drafts());

        tabs.get_mut(0).unwrap().toggle_drafts();
        assert!(!tabs.get(0).unwrap().show_drafts());
    }

    /// Session-only. Persisting would mean a keystroke silently editing a
    /// file the user hand-maintains.
    #[test]
    fn sort_and_draft_state_start_from_config_each_session() {
        let sort = Sort {
            column: Column::Author,
            order: Order::Asc,
            drafts_last: true,
        };
        let tabs = Tabs::new(&filters(&["One"]), sort, true, None);
        let tab = tabs.get(0).unwrap();

        assert_eq!(tab.sort_column, Column::Author);
        assert_eq!(tab.sort_order, Order::Asc);
        assert!(tab.show_drafts());
    }

    #[test]
    fn inverting_sort_flips_the_order_only() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();
        let column = tab.sort_column;

        tab.invert_sort();
        assert_eq!(tab.sort_order, Order::Asc);
        assert_eq!(tab.sort_column, column, "the column is unchanged");

        tab.invert_sort();
        assert_eq!(tab.sort_order, Order::Desc);
    }

    #[test]
    fn a_snapshot_replaces_the_previous_one() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_rows(vec![mr("a", "someone")], Instant::now());
        assert_eq!(tab.all().len(), 1);
        assert_eq!(tab.state, FetchState::Loaded);
        assert!(tab.fetched_at.is_some());

        tab.apply_rows(vec![mr("a", "someone"), mr("b", "someone")], Instant::now());
        assert_eq!(tab.all().len(), 2);
    }

    /// Everything is new on the first fetch, so nothing is.
    #[test]
    fn the_first_fetch_reports_nothing_as_new() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        let arrived = tab.apply_rows(vec![mr("a", "someone"), mr("b", "someone")], Instant::now());

        assert!(arrived.is_empty());
        assert!(!tab.is_new("a"));
    }

    #[test]
    fn later_fetches_report_only_what_arrived() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_rows(vec![mr("a", "someone")], Instant::now());
        let arrived = tab.apply_rows(vec![mr("a", "someone"), mr("b", "someone")], Instant::now());

        assert_eq!(arrived, ["b"]);
        assert!(tab.is_new("b"));
        assert!(!tab.is_new("a"), "already seen");
    }

    /// The cache-warm render is silent, so it must leave no `*` markers
    /// either — including over a tab that already had some. `warm` only ever runs on a
    /// fresh tab today, so this pins the contract rather than the current call order.
    #[test]
    fn cached_rows_clear_any_existing_new_markers() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_rows(vec![mr("a", "someone")], Instant::now());
        tab.apply_rows(vec![mr("a", "someone"), mr("b", "someone")], Instant::now());
        assert!(tab.is_new("b"), "the marker is there to be cleared");

        tab.apply_cached(
            vec![mr("a", "someone"), mr("b", "someone")],
            std::time::Duration::from_secs(60),
            false,
            Fragment::full(),
        );

        assert!(!tab.is_new("b"), "a warm render marks nothing as new");
        assert!(!tab.is_new("a"));
    }

    /// The marker's lifetime is one refresh cycle: a fetch that brings nothing new clears
    /// it, because each fetch replaces the set rather than adding to it.
    #[test]
    fn a_later_fetch_with_no_arrivals_clears_the_markers() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();
        let rows = vec![mr("a", "someone"), mr("b", "someone")];

        tab.apply_rows(vec![mr("a", "someone")], Instant::now());
        tab.apply_rows(rows.clone(), Instant::now());
        assert!(tab.is_new("b"));

        tab.apply_rows(rows, Instant::now());
        assert!(!tab.is_new("b"), "nothing arrived, so nothing is marked");
    }

    /// Selecting a row drops its `*`, and only its own.
    #[test]
    fn marking_a_row_seen_leaves_the_other_markers_alone() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_rows(vec![mr("a", "x")], Instant::now());
        tab.apply_rows(
            vec![mr("a", "x"), mr("b", "x"), mr("c", "x")],
            Instant::now(),
        );
        assert!(tab.is_new("b") && tab.is_new("c"));

        tab.mark_seen("b");

        assert!(!tab.is_new("b"));
        assert!(
            tab.is_new("c"),
            "looking at one arrival says nothing about the others"
        );
    }

    #[test]
    fn marking_an_unknown_id_seen_is_a_no_op() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();
        tab.apply_rows(vec![mr("a", "x")], Instant::now());
        tab.apply_rows(vec![mr("a", "x"), mr("b", "x")], Instant::now());

        tab.mark_seen("nonexistent");
        assert!(tab.is_new("b"));
    }

    /// And a fetch that brings something else moves the marker rather than accumulating.
    #[test]
    fn markers_describe_the_latest_refresh_only() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_rows(vec![mr("a", "x")], Instant::now());
        tab.apply_rows(vec![mr("a", "x"), mr("b", "x")], Instant::now());
        tab.apply_rows(
            vec![mr("a", "x"), mr("b", "x"), mr("c", "x")],
            Instant::now(),
        );

        assert!(tab.is_new("c"));
        assert!(!tab.is_new("b"), "b arrived in the previous cycle");
    }

    /// A transient error must not blank the table.
    #[test]
    fn a_failed_fetch_keeps_the_previous_snapshot() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_rows(vec![mr("a", "someone")], Instant::now());
        tab.apply_error(&Error::Http { status: 503 });

        assert_eq!(tab.all().len(), 1, "the snapshot survives");
        assert!(tab.state.is_failed());
        assert!(tab.needs_attention());
    }

    #[test]
    fn fetch_states_progress_as_expected() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        assert_eq!(tab.state, FetchState::Idle);
        tab.begin_fetch();
        assert!(tab.state.is_fetching());

        tab.apply_rows(Vec::new(), Instant::now());
        assert_eq!(tab.state, FetchState::Loaded);
        assert!(!tab.needs_attention());
    }

    /// A degraded query means fields are missing, which the UI has to be able to say.
    #[test]
    fn a_degraded_query_marks_the_tab() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();
        tab.apply_rows(Vec::new(), Instant::now());
        assert!(!tab.needs_attention());

        tab.fragment = Fragment::full().excluding(["userNotesCount"]);
        assert!(tab.needs_attention());
    }

    #[test]
    fn a_partial_response_marks_the_tab() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();
        tab.apply_rows(Vec::new(), Instant::now());

        tab.partial = true;
        assert!(tab.needs_attention());
    }

    fn snapshot(truncated: bool, partial: bool, fragment: Fragment) -> Snapshot {
        Snapshot {
            merge_requests: vec![mr("a", "someone")],
            truncated,
            fragment,
            partial,
            anomalies: crate::gitlab::wire::Anomalies::default(),
        }
    }

    /// A capped list is a normal outcome, and marking it `!` would train the user to
    /// ignore the marker that means the response actually broke.
    #[test]
    fn a_truncated_list_is_recorded_without_marking_the_tab() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_snapshot(snapshot(true, false, Fragment::full()), Instant::now());

        assert!(tab.truncated);
        assert!(!tab.partial);
        assert!(!tab.needs_attention());
    }

    /// The three conditions a fetch reports travel separately, and each lands on its own
    /// field rather than sharing one.
    #[test]
    fn a_snapshot_carries_partiality_and_the_query_shape() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_snapshot(snapshot(false, true, Fragment::minimal()), Instant::now());

        assert!(tab.partial);
        assert!(!tab.truncated);
        assert!(tab.fragment.is_minimal());
        assert!(tab.needs_attention());
    }

    /// A recovered fetch has to clear the flags, or a tab stays marked for the session
    /// after the condition that marked it is gone.
    #[test]
    fn a_clean_snapshot_clears_the_previous_markers() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.apply_snapshot(snapshot(true, true, Fragment::minimal()), Instant::now());
        assert!(tab.needs_attention());

        tab.apply_snapshot(snapshot(false, false, Fragment::full()), Instant::now());
        assert!(!tab.partial);
        assert!(!tab.truncated);
        assert!(!tab.needs_attention());
    }

    /// A refresh that reorders rows must not move the cursor onto a
    /// different merge request, so selection is an id.
    #[test]
    fn selection_is_held_by_id() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();

        tab.select(Some("gid://gitlab/MergeRequest/1".into()));
        tab.apply_rows(vec![mr("b", "x"), mr("a", "x")], Instant::now());

        assert_eq!(
            tab.selected_id(),
            Some("gid://gitlab/MergeRequest/1"),
            "reordering does not touch the selection"
        );
    }

    #[test]
    fn an_empty_filter_list_yields_no_tabs() {
        let mut tabs = Tabs::new(&[], Sort::default(), false, None);

        assert!(tabs.is_empty());
        assert!(tabs.active().is_none());
        assert!(!tabs.activate(0));

        // Cycling an empty set must not panic or divide by zero.
        tabs.next();
        tabs.previous();
        assert_eq!(tabs.active_index(), 0);
    }

    #[test]
    fn filter_state_is_carried_onto_the_tab() {
        let mut filter = Filter::named("Merged", Scope::Assigned);
        filter.state = StateFilter::Merged;

        let tabs = Tabs::new(&[filter], Sort::default(), false, None);
        assert_eq!(tabs.get(0).unwrap().name, "Merged");
    }
}
