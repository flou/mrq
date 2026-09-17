//! Turning keys into actions, and actions into state changes.
//!
//! Two things live here that are easy to get wrong elsewhere.
//!
//! # Effects are returned, not performed
//!
//! Opening a browser, writing to the clipboard and asking for a refresh all reach
//! outside the process. [`dispatch`] returns them as an [`Effect`] for the caller to
//! perform, which keeps the whole of this module a pure function of `(state, action)` —
//! testable without a terminal, a subprocess or a network.
//!
//! # The match is exhaustive on purpose
//!
//! Every [`Action`] is handled explicitly. A wildcard arm would mean a newly added
//! action silently does nothing, which presents as "that key is broken" with no compile
//! error and no failing test.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::state::Tabs;
use crate::config::keymap::{Action, Keymap};
use crate::config::schema::Column;
use crate::gitlab::fetch::Snapshot;
use crate::gitlab::model::MergeRequest;
use crate::logging::LogBuffer;
use crate::ui::skins;
use crate::ui::theme::Theme;

/// Which overlay, if any, currently has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    Help,
    Sort,
    Filter,
    Skin,
    Log,
}

impl Popup {
    /// The action that opens this popup, so the same key can close it again.
    const fn opening_action(self) -> Action {
        match self {
            Self::Help => Action::Help,
            Self::Sort => Action::SortMenu,
            Self::Filter => Action::FilterMenu,
            Self::Skin => Action::SkinMenu,
            Self::Log => Action::LogMenu,
        }
    }
}

/// An open popup: which one, and where the user is inside it.
///
/// One struct for all of them rather than a variant each. `cursor`, `scroll` and `query`
/// mean the same thing in every popup that has them, and the key handling is shared —
/// a variant each would mean a copy each of "j moves down, clamped".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupState {
    pub kind: Popup,
    /// The row the popup is on: the selection in the list popups, the scroll position
    /// in the text ones. One field because clamping and page movement are the same
    /// operation either way.
    pub cursor: usize,
    /// The filter switcher's fuzzy query.
    pub query: String,
    /// The log lines as they were when the popup opened.
    ///
    /// Snapshotted rather than read live: the buffer keeps filling while the popup is
    /// open, and a list that grows under the cursor cannot be read.
    pub lines: Vec<String>,
    /// The skin that was in force when the picker opened.
    ///
    /// The picker previews as the cursor moves, so cancelling has to put back something,
    /// and by then the theme no longer remembers what.
    pub previous_skin: Option<String>,
}

impl PopupState {
    pub const fn new(kind: Popup) -> Self {
        Self {
            kind,
            cursor: 0,
            query: String::new(),
            lines: Vec::new(),
            previous_skin: None,
        }
    }
}

/// What the keyboard is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing a search query. Printable keys go into the query, not to actions.
    Search {
        query: String,
    },
    Popup(PopupState),
}

impl Mode {
    #[cfg(test)]
    pub(crate) const fn is_normal(&self) -> bool {
        matches!(self, Self::Normal)
    }

    #[cfg(test)]
    pub(crate) const fn popup(&self) -> Option<Popup> {
        match self {
            Self::Popup(state) => Some(state.kind),
            Self::Normal | Self::Search { .. } => None,
        }
    }

    pub const fn popup_state(&self) -> Option<&PopupState> {
        match self {
            Self::Popup(state) => Some(state),
            Self::Normal | Self::Search { .. } => None,
        }
    }
}

/// Something that has to happen outside this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    RefreshAll,
    RefreshActive,
    /// Open a URL in the browser.
    Open(String),
    /// Put text on the clipboard.
    Copy(String),
    Quit,
}

/// The state actions operate on.
///
/// Deliberately excludes the terminal and the HTTP client: nothing an action does needs
/// them, and excluding them is what lets every action be tested directly. The theme is
/// here rather than with them because the skin picker changes it, and it is a value with
/// no terminal behind it.
#[derive(Debug)]
pub struct ViewState {
    pub tabs: Tabs,
    pub mode: Mode,
    /// Whether wide-only columns (`diff`, plus `[ui].wide_columns`) are shown, toggled
    /// by `w`. Session-only, like the theme picker, and shared across every tab: it is a
    /// layout choice, not a per-filter one.
    pub wide: bool,
    pub theme: Theme,
    pub drafts_last: bool,
    /// A transient message for the status bar.
    pub flash: Option<Flash>,
    /// The in-memory log ring, for the log popup.
    ///
    /// Reading it is not an outward effect, so it does not need to go through [`Effect`];
    /// holding the handle here is what lets the popup open with a snapshot of it.
    pub log: LogBuffer,
    /// How many table body rows the terminal currently shows.
    ///
    /// A renderer fact, but a scroll decision needs it: with the wrong window the table
    /// scrolls under a cursor that is still in the middle of the real one. `App` writes
    /// it at startup, on a resize and on every frame; [`HALF_PAGE_VIEWPORT`] stands in
    /// until the first of those.
    pub viewport: usize,
}

/// The columns the sort menu offers, in menu order.
///
/// Every column is sortable, so this is the display order rather than a subset.
pub const SORTABLE: [Column; 9] = Column::DEFAULT;

/// How long a transient message stays up.
///
/// Long enough to read, short enough that it is gone before the user wonders whether it
/// is a persistent condition.
pub const FLASH_TTL: Duration = Duration::from_secs(4);

/// A transient status-bar message and when it was raised.
///
/// Expiry is evaluated at render time rather than by a timer: a timer would need to know
/// to wake the loop, and the status bar's own clock tick already redraws every second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub message: String,
    pub at: Instant,
}

impl Flash {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            at: Instant::now(),
        }
    }

    pub fn is_live(&self, now: Instant) -> bool {
        now.duration_since(self.at) < FLASH_TTL
    }
}

impl ViewState {
    /// The rows the active tab currently shows, filtered and sorted.
    ///
    /// Borrowed rather than cloned: the draw path asks for this every frame, and every
    /// navigation key asks for it again to find the current selection.
    pub fn visible_rows(&self) -> Vec<&MergeRequest> {
        self.rows_of(self.tabs.active_index())
    }

    /// The rows one tab would show, in the order the user sees them.
    ///
    /// Not only the active tab: a background filter's snapshot has to be reconciled
    /// against the order *that* tab shows, not the one currently on screen.
    pub fn rows_of(&self, index: usize) -> Vec<&MergeRequest> {
        let Some(tab) = self.tabs.get(index) else {
            return Vec::new();
        };

        let mut rows: Vec<&MergeRequest> = self.shown_in(index).collect();
        crate::app::sort::sort(&mut rows, tab.sort_column, tab.sort_order, self.drafts_last);
        rows
    }

    /// How many rows a tab is showing.
    ///
    /// Not `rows_of(index).len()`: that still runs the full comparator over borrowed
    /// rows, to then discard everything but the length. The tab bar asks this of every
    /// tab on every frame, so it is the difference between a sort and a single pass.
    pub fn count_of(&self, index: usize) -> usize {
        self.shown_in(index).count()
    }

    /// How many of a tab's shown rows are marked as fresh arrivals.
    ///
    /// Shares `shown_in` with `count_of`, so the two never disagree about which rows a
    /// number refers to.
    pub fn new_count_of(&self, index: usize) -> usize {
        let tab = self.tabs.get(index);
        self.shown_in(index)
            .filter(|mr| tab.is_some_and(|t| t.is_new(&mr.id)))
            .count()
    }

    /// The merge requests a tab shows, unsorted and uncloned — the shared filter behind
    /// [`rows_of`](Self::rows_of) and [`count_of`](Self::count_of), so the two can never
    /// disagree about which rows are visible.
    fn shown_in(&self, index: usize) -> impl Iterator<Item = &MergeRequest> {
        let tab = self.tabs.get(index);
        let query = self.search_query_of(index);

        tab.into_iter().flat_map(move |tab| {
            tab.all()
                .iter()
                .filter(move |mr| tab.show_drafts() || !mr.draft)
                .filter(move |mr| query.is_none_or(|q| matches_search(mr, q)))
        })
    }

    /// The query in force, for the status bar to show.
    pub fn search_text(&self) -> Option<&str> {
        self.search_query()
    }

    /// The query in force: the one being typed, or the tab's committed one.
    fn search_query(&self) -> Option<&str> {
        self.search_query_of(self.tabs.active_index())
    }

    /// A half-typed search only applies to the tab being typed into.
    fn search_query_of(&self, index: usize) -> Option<&str> {
        let committed = self.tabs.get(index).and_then(|t| t.search.as_deref());
        match &self.mode {
            Mode::Search { query } if index == self.tabs.active_index() => Some(query.as_str()),
            Mode::Search { .. } | Mode::Normal | Mode::Popup(_) => committed,
        }
    }

    /// Apply a fetched snapshot, keeping the cursor on the same merge request.
    ///
    /// Selection is an id, so a reorder needs no work at all. A *removal*
    /// does: the id no longer resolves, and leaving it set would leave the cursor
    /// pointing at nothing. The replacement is the row that now sits where the old one
    /// was, which is what "nearest surviving row by previous index" means once the list
    /// has closed up behind it.
    ///
    /// Returns the ids that arrived, for the `*` markers and the notification rules.
    pub fn apply_snapshot(
        &mut self,
        filter: usize,
        snapshot: Snapshot,
        at: Instant,
    ) -> Vec<String> {
        let selected = self
            .tabs
            .get(filter)
            .and_then(|t| t.selected_id())
            .map(str::to_owned);
        // Taken before the rows are replaced: afterwards the old order is gone.
        let previous_index = selected
            .as_ref()
            .and_then(|id| self.rows_of(filter).iter().position(|mr| &mr.id == id));

        let Some(tab) = self.tabs.get_mut(filter) else {
            return Vec::new();
        };
        let arrived = tab.apply_snapshot(snapshot, at);

        self.reconcile_selection(filter, selected.as_deref(), previous_index);
        arrived
    }

    /// Re-derive every tab's per-user flags once the identity probe lands, reporting
    /// whether anything changed.
    ///
    /// Every tab, not only cache-warmed ones: a live snapshot fetched before the probe
    /// landed was already derived correctly in the fetch path, so re-deriving it here is
    /// a no-op that reports no change — and the handler stays correct whatever order the
    /// events arrive in.
    ///
    /// No re-sort call: [`Self::rows_of`] sorts on every call and the draw path asks for
    /// it every frame, so returning `true` here — which marks the frame dirty — *is* the
    /// re-sort. Selection is tracked by id, so a reorder cannot lose it.
    pub fn identify(&mut self, current_user: &str) -> bool {
        let mut changed = false;
        for tab in self.tabs.iter_mut() {
            changed |= tab.rederive(current_user);
        }
        changed
    }

    fn reconcile_selection(
        &mut self,
        filter: usize,
        selected: Option<&str>,
        previous_index: Option<usize>,
    ) {
        // `rows` borrows the same tab data `get_mut` below needs exclusively, so every
        // read of it — including through `next` — has to happen before that borrow, not
        // interleaved with it as a `&mut Tab` held across both used to allow.
        let rows = self.rows_of(filter);

        // Still fetched: the id is the whole point. Checked against the unfiltered set
        // rather than the visible rows, so a search that happens to be hiding the
        // selected row does not throw the selection away.
        let still_present = selected.is_some_and(|id| {
            self.tabs
                .get(filter)
                .is_some_and(|tab| tab.all().iter().any(|mr| mr.id == id))
        });
        if still_present {
            return;
        }

        // An empty set has no row to fall back to, and a selection pointing into it is a
        // phantom the table would try to draw a cursor on. No previous index means there
        // was nothing to hold a place for — either nothing was ever selected, or the tab
        // was empty last time around. Either way, the top row is the closest thing to
        // "where the cursor was".
        let next = if rows.is_empty() {
            None
        } else {
            let index = previous_index.unwrap_or(0).min(rows.len() - 1);
            Some(rows[index].id.clone())
        };

        if let Some(tab) = self.tabs.get_mut(filter) {
            tab.select(next);
        }
    }

    /// Select the top row of every tab that has rows but no selection yet.
    ///
    /// The live-fetch path gets its first selection from [`Self::apply_snapshot`]; the
    /// cache warm start bypasses it by setting rows directly on `Tab` before a
    /// `ViewState` exists to sort and filter them, so it needs this run once at startup.
    pub fn select_initial_rows(&mut self) {
        for index in 0..self.tabs.len() {
            self.reconcile_selection(index, None, None);
        }
    }

    /// The selected merge request, if the selection still resolves to a visible row.
    ///
    /// `shown_in` rather than `visible_rows`: finding one row by id needs neither the
    /// sort nor the `Vec` allocation that building the full visible list would do.
    pub fn selected(&self) -> Option<MergeRequest> {
        let id = self.tabs.active()?.selected_id()?;
        self.shown_in(self.tabs.active_index())
            .find(|mr| mr.id == id)
            .cloned()
    }

    /// Sanitised here, once, rather than at each call site: several build the message
    /// from an `Error`'s `Display` (`open failed: {error}`, `copy failed: {failure}`),
    /// and an `Error`'s formatting is free to use an em dash — correct for stderr and the
    /// log file, wrong for an ascii-theme status bar.
    pub fn flash(&mut self, message: impl Into<String>) {
        let message = self.theme.ascii_safe(&message.into()).into_owned();
        self.flash = Some(Flash::new(message));
    }

    /// The transient message, if it has not expired yet.
    pub fn live_flash(&self, now: Instant) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|flash| flash.is_live(now))
            .map(|flash| flash.message.as_str())
    }

    /// Move the cursor by `delta` rows, holding the selection by id.
    fn move_selection(&mut self, delta: isize) -> bool {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return false;
        }

        let current = self
            .tabs
            .active()
            .and_then(|t| t.selected_id())
            .and_then(|id| rows.iter().position(|mr| mr.id == id));

        let next = match current {
            None => 0,
            Some(index) => (index as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        };
        // Extracted before `rows` is used no further: `rows` borrows `self`, and
        // `select_row`/`set_scroll` need `&mut self`, so the borrow has to end here.
        let id = rows[next].id.clone();
        let len = rows.len();

        self.select_row(id);
        self.set_scroll(next, len);
        true
    }

    fn jump(&mut self, to_end: bool) -> bool {
        let rows = self.visible_rows();
        let Some(target) = (if to_end { rows.last() } else { rows.first() }) else {
            return false;
        };
        let id = target.id.clone();
        let index = if to_end { rows.len() - 1 } else { 0 };
        // Same reason as `move_selection`: `rows` borrows `self`, so its last use has to
        // come before the `&mut self` calls below.
        let len = rows.len();

        self.select_row(id);
        self.set_scroll(index, len);
        true
    }

    /// Move the scroll offset after a user navigation.
    ///
    /// Uses the window `App` last reported; before the first frame that is the
    /// half-page fallback, which keeps the selection visible without pretending to know
    /// the height.
    fn set_scroll(&mut self, cursor: usize, rows: usize) {
        let viewport = self.viewport;
        if let Some(tab) = self.tabs.active_mut() {
            tab.scroll = scroll_to_keep(tab.scroll, cursor, rows, viewport);
        }
    }

    /// How many rows `ctrl-d`/`ctrl-u` move: half the real table window, so the jump
    /// scales with the terminal instead of always being [`HALF_PAGE`] rows.
    fn half_page(&self) -> isize {
        isize::try_from(self.viewport / 2)
            .unwrap_or(HALF_PAGE)
            .max(1)
    }

    /// Move the cursor to a row because the *user* navigated there.
    ///
    /// Distinct from `Tab::select` alone, which `reconcile_selection` uses after a fetch:
    /// the `*` marker drops once the row "is selected", meaning the user went to look at
    /// it. A cursor that merely landed there because the previously selected row was
    /// removed has shown them nothing, so that path must not clear the marker.
    fn select_row(&mut self, id: String) {
        if let Some(tab) = self.tabs.active_mut() {
            tab.mark_seen(&id);
            tab.select(Some(id));
        }
    }
}

/// Whether a merge request matches a search query.
///
/// Title, author and repo only. Branch names would make a query like "main" match every
/// row, and ids are not something anyone types.
pub fn matches_search(mr: &MergeRequest, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let needle = query.to_lowercase();
    mr.title.to_lowercase().contains(&needle)
        || mr.author.username.to_lowercase().contains(&needle)
        || mr.project_name.to_lowercase().contains(&needle)
}

/// How many rows a half-page jump moves before the renderer has reported a real
/// viewport height. [`ViewState::half_page`] refines it once `viewport` is known.
const HALF_PAGE: isize = 10;

/// The value [`ViewState::viewport`] starts at, until `App` reports the real table
/// height.
///
/// [`scroll_to_keep`] needs a window to reason about, and this is the only one on hand
/// before the first frame.
pub const HALF_PAGE_VIEWPORT: usize = HALF_PAGE as usize;

/// Rows a wheel notch moves the cursor, when mouse support is on.
///
/// Small enough to feel like scrolling rather than paging, large enough that a flick is
/// not dozens of presses.
const MOUSE_WHEEL_ROWS: isize = 3;

/// Rows of context kept visible around the cursor while scrolling.
///
/// "Sensible margin": enough to see the rows the cursor passed, smaller than the
/// window so half the screen is never empty on a one-line move.
const SCROLL_MARGIN: usize = 2;

/// The scroll offset that keeps `cursor` in view within a `viewport`-row window.
///
/// The window does not move until the cursor reaches [`SCROLL_MARGIN`] of its edge,
/// so browsing down a long list scrolls in single rows rather than jumping the moment
/// the cursor leaves the first screenful. At either end the window is pinned.
pub fn scroll_to_keep(scroll: usize, cursor: usize, rows: usize, viewport: usize) -> usize {
    if rows <= viewport || rows == 0 || viewport == 0 {
        return 0;
    }
    let margin = SCROLL_MARGIN.min(viewport / 2);
    let last_start = rows - viewport;

    if cursor < scroll + margin {
        return cursor.saturating_sub(margin);
    }
    if cursor >= scroll + viewport - margin {
        return (cursor + 1)
            .saturating_sub(viewport - margin)
            .min(last_start);
    }
    scroll.min(last_start)
}

/// Apply one action.
///
/// Returns whether the screen needs redrawing, and anything the caller must perform.
pub fn dispatch(state: &mut ViewState, action: Action) -> (bool, Effect) {
    // An action clears the last transient message; leaving it up after the user has
    // moved on makes it look like a persistent condition.
    state.flash = None;

    match action {
        Action::Quit => (true, Effect::Quit),
        // A manual refresh resets the `*` markers on the tabs it asks to refresh,
        // whether or not the fetch that follows succeeds.
        Action::Refresh => {
            for tab in state.tabs.iter_mut() {
                tab.clear_new();
            }
            (true, Effect::RefreshAll)
        }
        Action::RefreshVisible => {
            if let Some(tab) = state.tabs.active_mut() {
                tab.clear_new();
            }
            (true, Effect::RefreshActive)
        }

        Action::Down => (state.move_selection(1), Effect::None),
        Action::Up => (state.move_selection(-1), Effect::None),
        Action::PageDown => (state.move_selection(state.half_page()), Effect::None),
        Action::PageUp => (state.move_selection(-state.half_page()), Effect::None),
        Action::Top => (state.jump(false), Effect::None),
        Action::Bottom => (state.jump(true), Effect::None),

        Action::OpenMr => match state.selected() {
            Some(mr) => (true, Effect::Open(mr.web_url)),
            None => (false, Effect::None),
        },
        Action::OpenPipeline => match state.selected().and_then(|mr| mr.pipeline) {
            Some(pipeline) => (true, Effect::Open(pipeline.url)),
            // Say so in the status bar and ring nothing.
            None => {
                state.flash("no pipeline");
                (true, Effect::None)
            }
        },
        Action::OpenProject => match state.selected() {
            Some(mr) => {
                // Derived from the merge request URL rather than carried separately:
                // every MR URL ends in /-/merge_requests/<iid>, and the project page is
                // what precedes it.
                let project = mr
                    .web_url
                    .split("/-/merge_requests/")
                    .next()
                    .unwrap_or(&mr.web_url)
                    .to_owned();
                (true, Effect::Open(project))
            }
            None => (false, Effect::None),
        },

        Action::CopyUrl => match state.selected() {
            Some(mr) => {
                state.flash("copied URL");
                (true, Effect::Copy(mr.web_url))
            }
            None => (false, Effect::None),
        },
        Action::CopyBranch => match state.selected() {
            Some(mr) => {
                state.flash("copied branch");
                (true, Effect::Copy(mr.source_branch))
            }
            None => (false, Effect::None),
        },

        Action::ToggleDrafts => {
            if let Some(tab) = state.tabs.active_mut() {
                tab.toggle_drafts();
            }
            (true, Effect::None)
        }
        Action::ToggleWide => {
            state.wide = !state.wide;
            (true, Effect::None)
        }
        Action::InvertSort => {
            if let Some(tab) = state.tabs.active_mut() {
                tab.invert_sort();
            }
            (true, Effect::None)
        }
        Action::SortMenu => {
            let mut popup = PopupState::new(Popup::Sort);
            // Opens on the column in force, so Enter without moving is a no-op rather
            // than a silent re-sort by whatever happened to be first.
            popup.cursor = state
                .tabs
                .active()
                .and_then(|tab| SORTABLE.iter().position(|c| *c == tab.sort_column))
                .unwrap_or(0);
            state.mode = Mode::Popup(popup);
            (true, Effect::None)
        }
        Action::FilterMenu => {
            let mut popup = PopupState::new(Popup::Filter);
            popup.cursor = state.tabs.active_index();
            state.mode = Mode::Popup(popup);
            (true, Effect::None)
        }
        Action::SkinMenu => {
            let mut popup = PopupState::new(Popup::Skin);
            // Opens on the skin in force, so the list reads as "you are here" and Esc
            // from an untouched picker previews nothing.
            popup.cursor = skins::BUILTIN_NAMES
                .iter()
                .position(|name| *name == state.theme.skin())
                .unwrap_or(0);
            popup.previous_skin = Some(state.theme.skin().to_owned());
            state.mode = Mode::Popup(popup);
            (true, Effect::None)
        }
        Action::Help => {
            state.mode = Mode::Popup(PopupState::new(Popup::Help));
            (true, Effect::None)
        }
        Action::LogMenu => {
            let mut popup = PopupState::new(Popup::Log);
            popup.lines = state.log.lines();
            // Opens at the end: the newest line is the one you came to read.
            popup.cursor = popup.lines.len().saturating_sub(1);
            state.mode = Mode::Popup(popup);
            (true, Effect::None)
        }

        Action::NextFilter => {
            state.tabs.next();
            (true, Effect::None)
        }
        Action::PrevFilter => {
            state.tabs.previous();
            (true, Effect::None)
        }

        Action::Search => {
            let existing = state
                .tabs
                .active()
                .and_then(|t| t.search.clone())
                .unwrap_or_default();
            state.mode = Mode::Search { query: existing };
            (true, Effect::None)
        }
        Action::ClearSearch => {
            state.mode = Mode::Normal;
            if let Some(tab) = state.tabs.active_mut() {
                tab.search = None;
            }
            (true, Effect::None)
        }
    }
}

/// What a key did, when it was handled by a mode rather than dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Nothing recognised it.
    Ignored,
    /// Handled; the screen may need redrawing.
    Handled { redraw: bool },
    /// Dispatched to an action, which produced an effect.
    Action { redraw: bool, effect: Effect },
}

/// Route a key press.
///
/// Modes capture keys before the keymap sees them: while typing a search, `d` is a
/// letter, not the draft toggle. Getting this wrong makes the search box unusable in a
/// way that looks like random misbehaviour.
pub fn handle_key(state: &mut ViewState, keymap: &Keymap, key: KeyEvent) -> KeyOutcome {
    match &state.mode {
        Mode::Search { query } => {
            let mut query = query.clone();
            match key.code {
                KeyCode::Esc => {
                    state.mode = Mode::Normal;
                    if let Some(tab) = state.tabs.active_mut() {
                        tab.search = None;
                    }
                    return KeyOutcome::Handled { redraw: true };
                }
                KeyCode::Enter => {
                    state.mode = Mode::Normal;
                    if let Some(tab) = state.tabs.active_mut() {
                        tab.search = (!query.is_empty()).then_some(query);
                    }
                    return KeyOutcome::Handled { redraw: true };
                }
                KeyCode::Backspace => {
                    query.pop();
                }
                // Ctrl-anything is not text; let it fall through so ctrl-c still quits
                // from inside the search box.
                KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return dispatch_key(state, keymap, key);
                }
                KeyCode::Char(c) => query.push(c),
                _ => return KeyOutcome::Ignored,
            }

            // The tab's query updates as you type, so the table filters live.
            if let Some(tab) = state.tabs.active_mut() {
                tab.search = Some(query.clone());
            }
            state.mode = Mode::Search { query };
            KeyOutcome::Handled { redraw: true }
        }

        Mode::Popup(popup) => {
            let popup = popup.clone();
            // Esc closes any popup, always, before anything else looks at the key.
            if key.code == KeyCode::Esc {
                if let Some(skin) = &popup.previous_skin {
                    state.theme = state.theme.with_skin(skin);
                }
                state.mode = Mode::Normal;
                return KeyOutcome::Handled { redraw: true };
            }
            // Quit must work from inside a popup, or a modal bug traps the user.
            if keymap.action_for(key) == Some(Action::Quit) {
                return dispatch_key(state, keymap, key);
            }
            // The key that opened this popup closes it again, same as Esc — except
            // Filter, whose key doubles as ordinary query text and must keep typing.
            if popup.kind != Popup::Filter
                && keymap.action_for(key) == Some(popup.kind.opening_action())
            {
                if let Some(skin) = &popup.previous_skin {
                    state.theme = state.theme.with_skin(skin);
                }
                state.mode = Mode::Normal;
                return KeyOutcome::Handled { redraw: true };
            }
            popup_key(state, keymap, popup, key)
        }

        Mode::Normal => handle_normal(state, keymap, key),
    }
}

/// Keys in normal mode: the resolved keymap first, then the `1`..`9` tab shortcuts.
///
/// The number keys are positional shortcuts bound by tab order rather than by anything
/// in `[keys]`, so a digit that the user *has* bound keeps their binding
/// and only the unclaimed ones select a tab.
fn handle_normal(state: &mut ViewState, keymap: &Keymap, key: KeyEvent) -> KeyOutcome {
    if keymap.action_for(key).is_some() {
        return dispatch_key(state, keymap, key);
    }
    match digit_position(key) {
        Some(position) if state.tabs.activate_position(position) => {
            KeyOutcome::Handled { redraw: true }
        }
        _ => KeyOutcome::Ignored,
    }
}

/// A bracketed paste lands in the search box as one block: the whole string arrives in a
/// single state update, control characters stripped so a trailing newline cannot corrupt
/// the single-line status bar, and it is ignored anywhere else.
pub fn handle_paste(state: &mut ViewState, text: &str) -> KeyOutcome {
    let Mode::Search { query } = &state.mode else {
        return KeyOutcome::Ignored;
    };
    let mut query = query.clone();
    query.extend(text.chars().filter(|c| !c.is_control()));
    if let Some(tab) = state.tabs.active_mut() {
        tab.search = Some(query.clone());
    }
    state.mode = Mode::Search { query };
    KeyOutcome::Handled { redraw: true }
}

/// Wheel-scroll moves the cursor, exactly like the keyboard: the view follows the
/// selection through [`ViewState::set_scroll`], so the cursor never leaves the visible
/// margin — a scroll that left the selection behind would contradict it.
pub fn mouse_wheel(state: &mut ViewState, up: bool) -> bool {
    let delta = if up {
        -MOUSE_WHEEL_ROWS
    } else {
        MOUSE_WHEEL_ROWS
    };
    state.move_selection(delta)
}

/// Select the row under a mouse click; the index is into the sorted, filtered rows the
/// table shows, so the caller folds the scroll offset into it.
pub fn mouse_select(state: &mut ViewState, index: usize) -> bool {
    let rows = state.visible_rows();
    let Some(mr) = rows.get(index) else {
        return false;
    };
    // Extracted before `rows` is used no further: it borrows `state`, and the calls
    // below need `&mut state`.
    let id = mr.id.clone();
    let len = rows.len();

    state.select_row(id);
    state.set_scroll(index, len);
    true
}

/// The merge request under a mouse click on its title cell.
pub fn mouse_open(state: &ViewState, index: usize) -> Option<Effect> {
    state
        .visible_rows()
        .get(index)
        .map(|mr| Effect::Open(mr.web_url.clone()))
}

/// The tab a `1`..`9` key addresses, if it is a plain digit with no modifiers.
fn digit_position(key: KeyEvent) -> Option<usize> {
    if !key.modifiers.is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => c.to_digit(10).map(|d| d as usize),
        _ => None,
    }
}

/// How many rows the list popups move on a page key. The renderer knows the real height;
/// this is what the key handler can assume without it.
const POPUP_PAGE: usize = 10;

/// Keys inside an open popup.
///
/// Every key is consumed whether or not it did something. A popup that let unhandled
/// keys through would have `j` scrolling the table behind it, which is the bug the
/// capture exists to prevent — the user cannot see what they are moving.
fn popup_key(
    state: &mut ViewState,
    keymap: &Keymap,
    mut popup: PopupState,
    key: KeyEvent,
) -> KeyOutcome {
    let rows = popup_rows(state, keymap, &popup);

    match key.code {
        KeyCode::Char('j') | KeyCode::Down => popup.cursor = next(popup.cursor, 1, rows),
        KeyCode::Char('k') | KeyCode::Up => popup.cursor = previous(popup.cursor, 1),
        KeyCode::PageDown => popup.cursor = next(popup.cursor, POPUP_PAGE, rows),
        KeyCode::PageUp => popup.cursor = previous(popup.cursor, POPUP_PAGE),
        KeyCode::Home => popup.cursor = 0,
        KeyCode::End => popup.cursor = rows.saturating_sub(1),

        KeyCode::Enter => {
            let kind = popup.kind;
            state.mode = Mode::Normal;
            return apply_popup(state, kind, &popup);
        }

        KeyCode::Backspace if popup.kind == Popup::Filter => {
            popup.query.pop();
            popup.cursor = 0;
        }
        KeyCode::Char(c) => match popup.kind {
            // Typing filters the list; there is no letter shortcut to collide with.
            Popup::Filter => {
                popup.query.push(c);
                popup.cursor = 0;
            }
            // A column's initial letter jumps to it.
            Popup::Sort => {
                if let Some(index) = SORTABLE
                    .iter()
                    .position(|column| column.header().to_lowercase().starts_with(c))
                {
                    popup.cursor = index;
                }
            }
            Popup::Skin => {
                if let Some(index) = skins::BUILTIN_NAMES
                    .iter()
                    .position(|name| name.starts_with(c))
                {
                    popup.cursor = index;
                }
            }
            Popup::Help | Popup::Log => return KeyOutcome::Handled { redraw: false },
        },

        _ => return KeyOutcome::Handled { redraw: false },
    }

    // The picker previews: the frame behind it is redrawn in the skin under the cursor,
    // because a list of names says nothing about what the table will look like.
    if popup.kind == Popup::Skin
        && let Some(name) = skins::BUILTIN_NAMES.get(popup.cursor)
    {
        state.theme = state.theme.with_skin(name);
    }

    state.mode = Mode::Popup(popup);
    KeyOutcome::Handled { redraw: true }
}

/// How many rows the popup has, for clamping the cursor.
///
/// The help popup counts the lines it will actually render — an unbounded cursor would
/// let `j` run past the end and then need as many `k` presses to come back.
fn popup_rows(state: &ViewState, keymap: &Keymap, popup: &PopupState) -> usize {
    match popup.kind {
        Popup::Sort => SORTABLE.len(),
        Popup::Filter => matching_filters(state, &popup.query).len(),
        Popup::Skin => skins::BUILTIN_NAMES.len(),
        Popup::Log => popup.lines.len(),
        Popup::Help => help_lines(keymap).len(),
    }
}

/// One line of the help popup, before it is styled or truncated.
///
/// Built here rather than in the renderer so the key handler can count the lines it is
/// scrolling through. Two implementations of the same layout would drift, and the one
/// that drifted would be the scroll bound — silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpLine {
    Blank,
    Heading(&'static str),
    Binding {
        keys: String,
        description: &'static str,
    },
}

/// The help popup's content, generated from the resolved keymap.
pub fn help_lines(keymap: &Keymap) -> Vec<HelpLine> {
    let mut lines = Vec::new();
    for category in crate::config::keymap::Category::ALL {
        let actions = keymap.bound_in(category);
        // A category whose every action is unbound is not a heading with nothing under
        // it, it is absent.
        if actions.is_empty() {
            continue;
        }

        if !lines.is_empty() {
            lines.push(HelpLine::Blank);
        }
        lines.push(HelpLine::Heading(category.title()));

        for action in actions {
            lines.push(HelpLine::Binding {
                keys: keymap
                    .keys_for(action)
                    .iter()
                    .map(|key| key.to_spec())
                    .collect::<Vec<_>>()
                    .join(", "),
                description: action.description(),
            });
        }
    }
    lines
}

fn next(cursor: usize, by: usize, rows: usize) -> usize {
    cursor.saturating_add(by).min(rows.saturating_sub(1))
}

const fn previous(cursor: usize, by: usize) -> usize {
    cursor.saturating_sub(by)
}

/// Indices of the filters matching the switcher's query, in tab order.
pub fn matching_filters(state: &ViewState, query: &str) -> Vec<usize> {
    (0..state.tabs.len())
        .filter(|index| {
            state
                .tabs
                .get(*index)
                .is_some_and(|tab| fuzzy_match(&tab.name, query))
        })
        .collect()
}

/// Subsequence match, case-insensitive: `plt` finds `Platform`.
///
/// Not a ranked fuzzy search. The list is the handful of filters someone configured, so
/// ordering them by score would reorder a list the user already knows by position.
fn fuzzy_match(name: &str, query: &str) -> bool {
    let mut haystack = name.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|needle| haystack.any(|c| c == needle))
}

/// Enter inside a popup.
fn apply_popup(state: &mut ViewState, kind: Popup, popup: &PopupState) -> KeyOutcome {
    match kind {
        Popup::Sort => {
            if let Some(column) = SORTABLE.get(popup.cursor).copied()
                && let Some(tab) = state.tabs.active_mut()
            {
                tab.sort_column = column;
            }
        }
        Popup::Filter => {
            if let Some(index) = matching_filters(state, &popup.query)
                .get(popup.cursor)
                .copied()
            {
                state.tabs.activate(index);
            }
        }
        // Already applied, one preview at a time; Enter is what stops Esc undoing it.
        Popup::Skin => state.flash(format!("skin: {}", state.theme.skin())),
        // Nothing to apply; Enter just closes them.
        Popup::Help | Popup::Log => {}
    }
    KeyOutcome::Handled { redraw: true }
}

fn dispatch_key(state: &mut ViewState, keymap: &Keymap, key: KeyEvent) -> KeyOutcome {
    // Unbound keys are ignored silently: logging per keystroke would fill the 50-line
    // popup with noise the moment someone leans on the keyboard.
    let Some(action) = keymap.action_for(key) else {
        return KeyOutcome::Ignored;
    };
    let (redraw, effect) = dispatch(state, action);
    KeyOutcome::Action { redraw, effect }
}

/// A theme for the tests below. Which skin it is does not matter to an action; what
/// matters is that building one needs no terminal.
#[cfg(test)]
fn test_theme() -> Theme {
    Theme::builtin(
        "catppuccin-mocha",
        false,
        &crate::term::caps::Capabilities {
            color: crate::term::caps::ColorDepth::TrueColor,
            hyperlinks: false,
            notify: crate::term::caps::NotifyEscape::None,
            focus_events: true,
            multiplexed: false,
            over_ssh: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Filter, Scope, Sort};
    use crate::gitlab::model::{Pipeline, PipelineStatus, User, fixtures::mr};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::BTreeMap;
    use std::time::Instant;

    fn keymap() -> Keymap {
        crate::config::keymap::resolve(&BTreeMap::new()).unwrap()
    }

    fn state_with(rows: Vec<MergeRequest>) -> ViewState {
        let mut tabs = Tabs::new(
            &[
                Filter::named("Assigned", Scope::Assigned),
                Filter::named("Reviewing", Scope::ReviewRequested),
            ],
            Sort::default(),
            true,
            None,
        );
        if let Some(tab) = tabs.active_mut() {
            tab.apply_rows(rows, Instant::now());
        }

        ViewState {
            tabs,
            mode: Mode::Normal,
            wide: false,
            theme: test_theme(),
            drafts_last: false,
            flash: None,
            log: LogBuffer::new(),
            viewport: HALF_PAGE_VIEWPORT,
        }
    }

    /// A flash message is sanitised for the active theme at the point it is raised, not
    /// left for each of `flash`'s several call sites to remember — several of them build
    /// the message from an `Error`'s `Display` (`open failed: {error}`), which is free to
    /// use an em dash. That is correct for stderr and the log file, and wrong for a
    /// status bar under the ascii theme.
    #[test]
    fn flash_is_sanitised_for_the_ascii_theme() {
        let mut state = state_with(vec![]);
        state.theme = Theme::builtin(
            "catppuccin-mocha",
            true,
            &crate::term::caps::Capabilities {
                color: crate::term::caps::ColorDepth::TrueColor,
                hyperlinks: false,
                notify: crate::term::caps::NotifyEscape::None,
                focus_events: true,
                multiplexed: false,
                over_ssh: false,
            },
        );

        state.flash("open failed: could not reach GitLab — timed out");

        let message = state.live_flash(Instant::now()).unwrap();
        assert!(!message.contains('—'), "{message}");
        assert!(
            message.contains("could not reach GitLab - timed out"),
            "{message}"
        );
    }

    /// Rows in an order the default sort preserves, so a test can reorder them and see
    /// the difference. The shared fixture gives every merge request the same
    /// `updated_at`, which would make "reordered" and "unchanged" indistinguishable.
    fn ordered(ids: &[&str]) -> Vec<MergeRequest> {
        ids.iter()
            .enumerate()
            .map(|(position, id)| {
                let mut m = mr(id, "someone");
                // Descending by update time is the default sort, so the first id given is
                // the most recently updated.
                m.updated_at = format!("2026-09-{:02}T00:00:00Z", 28 - position)
                    .parse()
                    .unwrap();
                m
            })
            .collect()
    }

    fn snapshot_of(rows: Vec<MergeRequest>) -> Snapshot {
        Snapshot {
            merge_requests: rows,
            truncated: false,
            fragment: crate::gitlab::query::Fragment::full(),
            partial: false,
            anomalies: crate::gitlab::wire::Anomalies::default(),
        }
    }

    /// The identity probe landing must re-derive cached flags and reorder an ASSIGNED
    /// sort with no explicit sort call: marking the frame dirty is the entire mechanism,
    /// because `rows_of` sorts on every call.
    #[test]
    fn identifying_rederives_cached_flags_and_reorders_an_assigned_sort() {
        // Written as if cached by "asmith": only "a" is assigned to that account.
        let mut a = mr("a", "someone");
        a.assignees = vec![User::new("carol")];
        a.recompute_derived("asmith");
        let mut b = mr("b", "someone");
        b.assignees = vec![User::new("asmith")];
        b.recompute_derived("asmith");

        let mut state = state_with(vec![a, b]);
        let tab = state.tabs.active_mut().unwrap();
        tab.sort_column = Column::Assigned;
        tab.sort_order = crate::config::schema::Order::Asc;

        // "asmith" is assigned "b", so it sorts first.
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );

        let changed = state.identify("carol");
        assert!(changed, "switching accounts must report a change");

        // "carol" is assigned "a" instead, and no sort was called explicitly.
        assert_eq!(
            state
                .visible_rows()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn identifying_with_the_same_account_reports_no_change() {
        let mut a = mr("a", "someone");
        a.recompute_derived("asmith");
        let mut state = state_with(vec![a]);

        assert!(!state.identify("asmith"));
    }

    fn selected_id(state: &ViewState) -> Option<String> {
        state
            .tabs
            .active()
            .and_then(|t| t.selected_id())
            .map(str::to_owned)
    }

    /// Tracking by index means a background refresh silently moves the
    /// cursor onto a different merge request, and `o` then opens the wrong one.
    #[test]
    fn a_refresh_that_reorders_rows_keeps_the_cursor_on_the_same_mr() {
        let mut state = state_with(ordered(&["a", "b", "c"]));
        state.tabs.active_mut().unwrap().select(Some("b".into()));
        assert_eq!(state.visible_rows()[1].id, "b");

        state.apply_snapshot(0, snapshot_of(ordered(&["c", "b", "a"])), Instant::now());

        assert_eq!(selected_id(&state).as_deref(), Some("b"));
        assert_eq!(state.selected().unwrap().id, "b", "and it still resolves");
    }

    /// The cursor lands on whatever now occupies the vacated position.
    #[test]
    fn a_removed_selection_moves_to_the_nearest_surviving_row() {
        let mut state = state_with(ordered(&["a", "b", "c", "d"]));
        state.tabs.active_mut().unwrap().select(Some("b".into()));

        state.apply_snapshot(0, snapshot_of(ordered(&["a", "c", "d"])), Instant::now());

        assert_eq!(
            selected_id(&state).as_deref(),
            Some("c"),
            "index 1 is now `c`"
        );
    }

    /// Removing the last row has no row at the previous index, so the clamp is what
    /// stops the selection pointing past the end.
    #[test]
    fn removing_the_last_row_selects_the_new_last_row() {
        let mut state = state_with(ordered(&["a", "b", "c"]));
        state.tabs.active_mut().unwrap().select(Some("c".into()));

        state.apply_snapshot(0, snapshot_of(ordered(&["a", "b"])), Instant::now());

        assert_eq!(selected_id(&state).as_deref(), Some("b"));
    }

    /// There is no id to hold a place for on the very first fetch, so the top row is the
    /// selection instead of leaving the cursor nowhere.
    #[test]
    fn the_first_fetch_selects_the_top_row() {
        let mut state = state_with(Vec::new());

        state.apply_snapshot(0, snapshot_of(ordered(&["a", "b", "c"])), Instant::now());

        assert_eq!(selected_id(&state).as_deref(), Some("a"));
    }

    /// The cache warm start sets rows directly on `Tab`, bypassing `apply_snapshot`, so
    /// it needs its own pass to give every populated tab a selection before first paint.
    #[test]
    fn select_initial_rows_selects_the_top_row_of_every_populated_tab() {
        let mut state = state_with(Vec::new());
        state
            .tabs
            .active_mut()
            .unwrap()
            .apply_rows(ordered(&["a", "b"]), Instant::now());
        state
            .tabs
            .get_mut(1)
            .unwrap()
            .apply_rows(ordered(&["x"]), Instant::now());

        state.select_initial_rows();

        assert_eq!(selected_id(&state).as_deref(), Some("a"));
        assert_eq!(state.tabs.get(1).unwrap().selected_id(), Some("x"));
    }

    #[test]
    fn select_initial_rows_leaves_an_empty_tab_unselected() {
        let mut state = state_with(Vec::new());

        state.select_initial_rows();

        assert_eq!(selected_id(&state), None);
    }

    /// An empty set has nothing to select, and a leftover id is a phantom the table
    /// would try to draw a cursor on.
    #[test]
    fn an_empty_result_leaves_no_phantom_selection() {
        let mut state = state_with(ordered(&["a", "b"]));
        state.tabs.active_mut().unwrap().select(Some("a".into()));

        state.apply_snapshot(0, snapshot_of(Vec::new()), Instant::now());

        assert_eq!(selected_id(&state), None);
        assert_eq!(state.selected(), None);
    }

    /// A background tab reconciles against its own order, not the visible tab's.
    #[test]
    fn a_background_tab_keeps_its_own_selection() {
        let mut state = state_with(ordered(&["a", "b"]));
        state
            .tabs
            .get_mut(1)
            .unwrap()
            .apply_rows(ordered(&["x", "y", "z"]), Instant::now());
        state.tabs.get_mut(1).unwrap().select(Some("y".into()));

        state.apply_snapshot(1, snapshot_of(ordered(&["x", "z"])), Instant::now());

        assert_eq!(state.tabs.get(1).unwrap().selected_id(), Some("z"));
        assert_eq!(
            state.tabs.active().unwrap().selected_id(),
            None,
            "the visible tab is untouched"
        );
    }

    /// A search hiding the selected row is not a removal — throwing the selection away
    /// would lose it the moment a refresh lands mid-search.
    #[test]
    fn a_search_hiding_the_selection_does_not_discard_it() {
        let mut state = state_with(ordered(&["a", "b", "c"]));
        state.tabs.active_mut().unwrap().select(Some("b".into()));
        state.tabs.active_mut().unwrap().search = Some("zzz-matches-nothing".into());
        assert!(state.visible_rows().is_empty());

        state.apply_snapshot(0, snapshot_of(ordered(&["a", "b", "c"])), Instant::now());

        assert_eq!(selected_id(&state).as_deref(), Some("b"));
    }

    /// Rows with no pipeline. The shared fixture carries one, and several tests here
    /// are about what happens when there is none.
    fn rows(n: usize) -> Vec<MergeRequest> {
        (0..n)
            .map(|i| {
                let mut m = mr(&format!("id-{i}"), "someone");
                m.title = format!("merge request {i}");
                m.web_url = format!("https://gl.example.com/g/p/-/merge_requests/{i}");
                m.source_branch = format!("branch-{i}");
                m.pipeline = None;
                m
            })
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// The selected row's position in the visible list, by id rather than a raw index —
    /// what the cursor actually is, as far as `move_selection` is concerned.
    fn cursor_of(state: &ViewState) -> usize {
        let id = state.selected().unwrap().id;
        state
            .visible_rows()
            .iter()
            .position(|mr| mr.id == id)
            .unwrap()
    }

    /// Every action is named; a wildcard arm would let a new one silently do nothing,
    /// which presents as "that key is broken".
    #[test]
    fn every_action_is_handled() {
        for action in Action::ALL {
            let mut state = state_with(rows(3));
            // Must not panic, and must reach a real arm.
            let _ = dispatch(&mut state, action);
        }
    }

    #[test]
    fn navigation_moves_the_selection_by_id() {
        let mut state = state_with(rows(5));

        dispatch(&mut state, Action::Down);
        let first = state.selected().unwrap().id;

        dispatch(&mut state, Action::Down);
        assert_ne!(state.selected().unwrap().id, first);

        dispatch(&mut state, Action::Up);
        assert_eq!(state.selected().unwrap().id, first);
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let mut state = state_with(rows(3));

        for _ in 0..10 {
            dispatch(&mut state, Action::Down);
        }
        let last = state.selected().unwrap().id;

        for _ in 0..10 {
            dispatch(&mut state, Action::Up);
        }
        let first = state.selected().unwrap().id;

        assert_ne!(first, last);
        dispatch(&mut state, Action::Up);
        assert_eq!(state.selected().unwrap().id, first, "clamped, not wrapped");
    }

    #[test]
    fn top_and_bottom_jump_to_the_ends() {
        let mut state = state_with(rows(10));

        dispatch(&mut state, Action::Bottom);
        let bottom = state.selected().unwrap().id;
        dispatch(&mut state, Action::Top);
        let top = state.selected().unwrap().id;

        assert_ne!(top, bottom);
        assert_eq!(top, state.visible_rows()[0].id);
    }

    #[test]
    fn navigating_an_empty_list_is_a_no_op() {
        let mut state = state_with(Vec::new());

        assert_eq!(dispatch(&mut state, Action::Down), (false, Effect::None));
        assert_eq!(dispatch(&mut state, Action::Top), (false, Effect::None));
        assert!(state.selected().is_none());
    }

    #[test]
    fn opening_a_merge_request_returns_its_url() {
        let mut state = state_with(rows(3));
        dispatch(&mut state, Action::Down);

        let (_, effect) = dispatch(&mut state, Action::OpenMr);
        match effect {
            Effect::Open(url) => assert!(url.contains("/-/merge_requests/"), "{url}"),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    /// Flash it in the status bar and ring nothing.
    #[test]
    fn opening_a_pipeline_that_does_not_exist_says_so() {
        let mut state = state_with(rows(1));
        dispatch(&mut state, Action::Down);

        let (redraw, effect) = dispatch(&mut state, Action::OpenPipeline);
        assert_eq!(effect, Effect::None);
        assert!(redraw);
        assert_eq!(
            state.flash.as_ref().map(|f| f.message.as_str()),
            Some("no pipeline")
        );
    }

    #[test]
    fn opening_a_pipeline_returns_its_url() {
        let mut list = rows(1);
        list[0].pipeline = Some(Pipeline {
            url: "https://gl.example.com/g/p/-/pipelines/9".into(),
            status: PipelineStatus::Success,
            finished_at: None,
        });
        let mut state = state_with(list);
        dispatch(&mut state, Action::Down);

        let (_, effect) = dispatch(&mut state, Action::OpenPipeline);
        assert_eq!(
            effect,
            Effect::Open("https://gl.example.com/g/p/-/pipelines/9".into())
        );
    }

    #[test]
    fn opening_the_project_trims_the_merge_request_path() {
        let mut state = state_with(rows(1));
        dispatch(&mut state, Action::Down);

        let (_, effect) = dispatch(&mut state, Action::OpenProject);
        assert_eq!(
            effect,
            Effect::Open("https://gl.example.com/g/p".into()),
            "the project page is what precedes /-/merge_requests/"
        );
    }

    #[test]
    fn copy_actions_return_what_to_copy_and_confirm() {
        let mut state = state_with(rows(1));
        dispatch(&mut state, Action::Down);

        let (_, effect) = dispatch(&mut state, Action::CopyUrl);
        assert!(matches!(effect, Effect::Copy(url) if url.contains("merge_requests")));
        assert_eq!(
            state.flash.as_ref().map(|f| f.message.as_str()),
            Some("copied URL")
        );

        let (_, effect) = dispatch(&mut state, Action::CopyBranch);
        assert_eq!(effect, Effect::Copy("branch-0".into()));
        assert_eq!(
            state.flash.as_ref().map(|f| f.message.as_str()),
            Some("copied branch")
        );
    }

    /// The `*` goes once the user has moved the cursor onto the row.
    #[test]
    fn navigating_onto_a_new_row_clears_its_marker() {
        let mut state = state_with(rows(3));
        {
            let tab = state.tabs.active_mut().unwrap();
            tab.apply_rows(rows(1), Instant::now());
            tab.apply_rows(rows(3), Instant::now());
        }
        let visible = state.visible_rows();
        let (first, second) = (visible[0].id.clone(), visible[1].id.clone());
        assert!(state.tabs.active().unwrap().is_new(&second));

        // Down twice: onto the first row, then onto the second.
        dispatch(&mut state, Action::Down);
        dispatch(&mut state, Action::Down);

        let tab = state.tabs.active().unwrap();
        assert_eq!(tab.selected_id(), Some(second.as_str()));
        assert!(!tab.is_new(&second), "the row the cursor is on");
        assert!(!tab.is_new(&first), "and the one it passed through");
    }

    /// A cursor that lands on a row because the previous selection was removed has shown
    /// the user nothing, so it must not eat the marker.
    #[test]
    fn a_reconciled_selection_does_not_clear_a_marker() {
        let mut state = state_with(rows(3));
        {
            let tab = state.tabs.active_mut().unwrap();
            tab.apply_rows(ordered(&["a", "b"]), Instant::now());
            tab.select(Some("a".into()));
        }

        // "a" disappears and "c" arrives; the cursor has to go somewhere.
        state.apply_snapshot(0, snapshot_of(ordered(&["b", "c"])), Instant::now());

        let tab = state.tabs.active().unwrap();
        assert!(tab.is_new("c"), "an arrival the user has not looked at");
        assert_ne!(tab.selected_id(), Some("a"), "the old row is gone");
    }

    #[test]
    fn new_count_follows_the_fresh_arrivals() {
        let mut state = state_with(rows(3));
        {
            let tab = state.tabs.active_mut().unwrap();
            tab.apply_rows(rows(1), Instant::now());
            tab.apply_rows(rows(3), Instant::now());
        }
        assert_eq!(
            state.new_count_of(0),
            2,
            "the two rows that arrived in the last refresh"
        );
        state.tabs.get_mut(0).unwrap().mark_seen("id-2");
        assert_eq!(state.new_count_of(0), 1, "seeing one arrival drops only it");
    }

    /// A manual refresh resets the markers, and does so at request time so
    /// they go even when the fetch that follows fails.
    #[test]
    fn a_manual_refresh_clears_the_markers_on_the_tabs_it_refreshes() {
        let mut state = state_with(rows(3));
        let mark = |state: &mut ViewState, index: usize| {
            let tab = state.tabs.get_mut(index).unwrap();
            tab.apply_rows(rows(1), Instant::now());
            tab.apply_rows(rows(3), Instant::now());
            state.rows_of(index)[1].id.clone()
        };

        let active = mark(&mut state, 0);
        assert!(state.tabs.get(0).unwrap().is_new(&active));

        assert_eq!(
            dispatch(&mut state, Action::RefreshVisible).1,
            Effect::RefreshActive
        );
        assert!(!state.tabs.get(0).unwrap().is_new(&active));

        // And ctrl-r clears every tab, not only the visible one.
        let again = mark(&mut state, 0);
        assert!(state.tabs.get(0).unwrap().is_new(&again));

        assert_eq!(dispatch(&mut state, Action::Refresh).1, Effect::RefreshAll);
        assert!(
            state.tabs.iter().all(|t| !t.is_new(&again)),
            "ctrl-r resets every tab"
        );
    }

    #[test]
    fn actions_with_no_selection_do_nothing() {
        let mut state = state_with(rows(3));

        for action in [
            Action::OpenMr,
            Action::OpenProject,
            Action::CopyUrl,
            Action::CopyBranch,
        ] {
            assert_eq!(
                dispatch(&mut state, action),
                (false, Effect::None),
                "{action:?}"
            );
        }
    }

    #[test]
    fn refresh_actions_return_the_right_scope() {
        let mut state = state_with(rows(1));

        assert_eq!(dispatch(&mut state, Action::Refresh).1, Effect::RefreshAll);
        assert_eq!(
            dispatch(&mut state, Action::RefreshVisible).1,
            Effect::RefreshActive
        );
    }

    #[test]
    fn view_toggles_flip_their_state() {
        let mut state = state_with(rows(1));

        let drafts = state.tabs.active().unwrap().show_drafts();
        dispatch(&mut state, Action::ToggleDrafts);
        assert_ne!(state.tabs.active().unwrap().show_drafts(), drafts);

        let wide = state.wide;
        dispatch(&mut state, Action::ToggleWide);
        assert_ne!(state.wide, wide);

        let order = state.tabs.active().unwrap().sort_order;
        dispatch(&mut state, Action::InvertSort);
        assert_ne!(state.tabs.active().unwrap().sort_order, order);
    }

    #[test]
    fn filter_navigation_cycles_tabs() {
        let mut state = state_with(rows(1));

        dispatch(&mut state, Action::NextFilter);
        assert_eq!(state.tabs.active_index(), 1);
        dispatch(&mut state, Action::PrevFilter);
        assert_eq!(state.tabs.active_index(), 0);
    }

    /// `1`..`9` jump by tab position; the tab bar advertises the number.
    #[test]
    fn number_keys_select_the_nth_filter() {
        let mut state = state_with(rows(1));
        let keymap = keymap();

        let outcome = handle_key(&mut state, &keymap, key(KeyCode::Char('2')));
        assert_eq!(outcome, KeyOutcome::Handled { redraw: true });
        assert_eq!(state.tabs.active_index(), 1);

        handle_key(&mut state, &keymap, key(KeyCode::Char('1')));
        assert_eq!(state.tabs.active_index(), 0);
    }

    /// `0` is not a position, and a digit for a tab that does not exist is ignored.
    #[test]
    fn out_of_range_number_keys_are_ignored() {
        let mut state = state_with(rows(1));
        let keymap = keymap();

        assert_eq!(
            handle_key(&mut state, &keymap, key(KeyCode::Char('9'))),
            KeyOutcome::Ignored,
            "only two filters exist"
        );
        assert_eq!(
            handle_key(&mut state, &keymap, key(KeyCode::Char('0'))),
            KeyOutcome::Ignored
        );
    }

    /// A digit the user binds in `[keys]` keeps its binding; only unclaimed digits
    /// select a tab, or the override would silently never fire.
    #[test]
    fn a_user_bound_digit_wins_over_the_shortcut() {
        use std::collections::BTreeMap;

        let mut user = BTreeMap::new();
        user.insert("toggle_drafts".to_owned(), vec!["1".to_owned()]);
        let keymap = crate::config::keymap::resolve(&user).unwrap();

        let mut state = state_with(rows(2));
        let before = state.tabs.active().unwrap().show_drafts();
        handle_key(&mut state, &keymap, key(KeyCode::Char('1')));

        assert_ne!(state.tabs.active().unwrap().show_drafts(), before);
        assert_eq!(state.tabs.active_index(), 0, "and the tab did not change");
    }

    /// Number keys only select tabs in normal mode; in the search box they are text.
    #[test]
    fn number_keys_type_in_search_mode() {
        let mut state = state_with(rows(1));
        let keymap = keymap();

        dispatch(&mut state, Action::Search);
        handle_key(&mut state, &keymap, key(KeyCode::Char('2')));

        assert_eq!(
            state.mode,
            Mode::Search { query: "2".into() },
            "the digit typed into the query"
        );
        assert_eq!(state.tabs.active_index(), 0);
    }

    /// A bracketed paste lands in the search box as one block: whole string in a single
    /// state update, control characters (a trailing newline, most likely) stripped, and
    /// the live filter re-run over it.
    #[test]
    fn a_paste_lands_atomically_in_the_search_box() {
        let mut state = state_with(rows(3));

        dispatch(&mut state, Action::Search);
        let outcome = handle_paste(&mut state, "merge req\nuest 2\n");

        assert_eq!(outcome, KeyOutcome::Handled { redraw: true });
        assert_eq!(
            state.mode,
            Mode::Search {
                query: "merge request 2".into(),
            },
            "controls stripped, everything else kept, one update"
        );
        assert_eq!(
            state.tabs.active().unwrap().search.as_deref(),
            Some("merge request 2")
        );
        let visible: Vec<String> = state
            .visible_rows()
            .into_iter()
            .map(|mr| mr.id.clone())
            .collect();
        assert_eq!(
            visible,
            ["id-2".to_owned()],
            "the live filter runs over the pasted query"
        );
    }

    /// Paste is a search-field feature; anywhere else it is ignored rather than, say,
    /// firing a draft toggle.
    #[test]
    fn a_paste_outside_the_search_box_is_ignored() {
        let mut state = state_with(rows(2));
        assert_eq!(
            handle_paste(&mut state, "MR-42"),
            KeyOutcome::Ignored,
            "normal mode"
        );
        assert_eq!(state.mode, Mode::Normal);

        dispatch(&mut state, Action::SortMenu);
        assert_eq!(
            handle_paste(&mut state, "MR-42"),
            KeyOutcome::Ignored,
            "popup mode"
        );
    }

    /// Pasting an empty or all-control block leaves no query but still counts as handled.
    #[test]
    fn an_empty_paste_changes_nothing() {
        let mut state = state_with(rows(2));
        dispatch(&mut state, Action::Search);

        let outcome = handle_paste(&mut state, "\n\n");
        assert_eq!(outcome, KeyOutcome::Handled { redraw: true });
        assert_eq!(
            state.mode,
            Mode::Search {
                query: String::new()
            }
        );
    }

    /// A click selects the row under it, and the view follows it so it stays visible.
    #[test]
    fn selecting_by_index_picks_that_row_and_follows_it_with_the_view() {
        let mut state = state_with(rows(30));
        let position = |state: &ViewState| {
            let id = state.selected().unwrap().id;
            state
                .visible_rows()
                .iter()
                .position(|mr| mr.id == id)
                .unwrap()
        };

        assert!(mouse_select(&mut state, 5));
        assert_eq!(position(&state), 5);
        assert_eq!(
            state.tabs.active().unwrap().scroll,
            0,
            "row 5 fits in the window"
        );

        assert!(mouse_select(&mut state, 20));
        let scroll = state.tabs.active().unwrap().scroll;
        assert_eq!(
            position(&state) - scroll,
            HALF_PAGE_VIEWPORT - SCROLL_MARGIN - 1,
            "the view rides the fallback window's margin to reach the click"
        );
    }

    /// A click below the last row or on nothing is ignored, not an error.
    #[test]
    fn selecting_out_of_range_or_empty_is_a_no_op() {
        let mut state = state_with(rows(2));
        assert!(!mouse_select(&mut state, 5), "no such row");
        assert!(state.selected().is_none());

        let mut empty = state_with(vec![]);
        assert!(!mouse_select(&mut empty, 0), "nothing to click");
        assert!(empty.selected().is_none());
    }

    /// Opening under a click yields the row's URL, and nothing when there is no row.
    #[test]
    fn opening_under_a_click_returns_the_row_url() {
        let state = state_with(rows(3));
        assert_eq!(
            mouse_open(&state, 1),
            Some(Effect::Open(
                "https://gl.example.com/g/p/-/merge_requests/1".to_owned()
            ))
        );
        assert_eq!(mouse_open(&state, 9), None, "no row there");
        assert_eq!(
            mouse_open(&state, 0),
            Some(Effect::Open(state.visible_rows()[0].web_url.clone()))
        );
    }

    /// The wheel moves the cursor a few rows per notch and pins at both ends, exactly
    /// like the keyboard, so it can never scroll the selection out from under the view.
    #[test]
    fn the_wheel_moves_three_rows_per_notch() {
        let mut state = state_with(rows(20));
        let position = |state: &ViewState| {
            let id = state.selected().unwrap().id;
            state
                .visible_rows()
                .iter()
                .position(|mr| mr.id == id)
                .unwrap()
        };

        assert!(mouse_wheel(&mut state, false));
        assert_eq!(
            position(&state),
            0,
            "the first notch lands on the first row"
        );
        assert!(mouse_wheel(&mut state, false));
        assert!(mouse_wheel(&mut state, false));
        assert_eq!(position(&state), 6);

        assert!(mouse_wheel(&mut state, true));
        assert_eq!(position(&state), 3);
    }

    #[test]
    fn the_wheel_pins_at_both_ends() {
        let mut state = state_with(rows(5));
        let position = |state: &ViewState| {
            let id = state.selected().unwrap().id;
            state
                .visible_rows()
                .iter()
                .position(|mr| mr.id == id)
                .unwrap()
        };

        for _ in 0..10 {
            assert!(mouse_wheel(&mut state, false));
        }
        assert_eq!(position(&state), 4, "pinned at the end");

        for _ in 0..10 {
            assert!(mouse_wheel(&mut state, true));
        }
        assert_eq!(position(&state), 0, "pinned at the start");
    }

    #[test]
    fn the_wheel_does_nothing_to_an_empty_list() {
        let mut state = state_with(vec![]);
        assert!(!mouse_wheel(&mut state, false));
        assert!(!mouse_wheel(&mut state, true));
    }

    /// Navigation keeps the selection visible: within the fallback window the offset
    /// does not move, and once the cursor reaches the edge margin the window follows so
    /// it never drops out of view.
    #[test]
    fn navigation_scrolls_to_keep_the_selection_visible() {
        let mut state = state_with(rows(40));
        let keymap = keymap();
        let scroll_of = |state: &ViewState| state.tabs.active().unwrap().scroll;

        for _ in 0..3 {
            handle_key(&mut state, &keymap, key(KeyCode::Char('j')));
        }
        assert_eq!(
            scroll_of(&state),
            0,
            "a cursor still inside the window does not move it"
        );

        for _ in 0..27 {
            handle_key(&mut state, &keymap, key(KeyCode::Char('j')));
        }
        let scrolled = scroll_of(&state);
        assert!(
            scrolled > 0,
            "past the window the table scrolls: {scrolled}"
        );

        let cursor = state
            .visible_rows()
            .iter()
            .position(|mr| mr.id == state.selected().unwrap().id)
            .unwrap();
        assert_eq!(
            cursor - scrolled,
            HALF_PAGE_VIEWPORT - SCROLL_MARGIN - 1,
            "the cursor rides the bottom margin of the window"
        );

        dispatch(&mut state, Action::Bottom);
        assert_eq!(
            scroll_of(&state),
            40 - HALF_PAGE_VIEWPORT,
            "pinned at the end"
        );

        dispatch(&mut state, Action::Top);
        assert_eq!(scroll_of(&state), 0, "pinned at the start");
    }

    /// The reported bug: the action layer assumed a ten-row window whatever the terminal
    /// was, so on a tall screen the table began scrolling at the ninth row while the
    /// cursor still had twenty rows of window beneath it. The cursor appeared frozen and
    /// the list slid under it until the last row came into view, which is where the
    /// fallback's clamp finally met the real one.
    #[test]
    fn a_tall_window_moves_the_cursor_before_it_moves_the_table() {
        let mut state = state_with(rows(100));
        state.viewport = 30;
        let keymap = keymap();

        // The first `j` only lands the cursor on row 0; each one after that is a real
        // step, so twelve presses reach row 11.
        for _ in 0..12 {
            handle_key(&mut state, &keymap, key(KeyCode::Char('j')));
        }

        assert_eq!(cursor_of(&state), 11, "eleven rows down");
        assert_eq!(
            state.tabs.active().unwrap().scroll,
            0,
            "a cursor inside a thirty-row window does not move it"
        );
    }

    #[test]
    fn the_table_scrolls_at_the_real_windows_margin() {
        let mut state = state_with(rows(100));
        state.viewport = 30;
        let keymap = keymap();
        let scroll_of = |state: &ViewState| state.tabs.active().unwrap().scroll;

        // 28 presses land the cursor on row 27, still inside a 30-row window with a
        // 2-row margin (the first press only selects row 0).
        for _ in 0..28 {
            handle_key(&mut state, &keymap, key(KeyCode::Char('j')));
        }
        assert_eq!(scroll_of(&state), 0, "row 27 is still inside the window");

        handle_key(&mut state, &keymap, key(KeyCode::Char('j')));
        assert_eq!(scroll_of(&state), 1, "row 28 reaches the bottom margin");
    }

    /// The wheel and a click run the same scroll decision as the keyboard, so they
    /// carried the same fault: a click well inside a tall window used to drag the list.
    #[test]
    fn a_click_inside_a_tall_window_does_not_move_it() {
        let mut state = state_with(rows(40));
        state.viewport = 30;

        assert!(mouse_select(&mut state, 20));
        assert_eq!(cursor_of(&state), 20);
        assert_eq!(state.tabs.active().unwrap().scroll, 0);
    }

    /// `ctrl-d`/`ctrl-u` scale with the real window instead of always moving the
    /// ten-row fallback.
    #[test]
    fn half_page_jumps_scale_with_the_real_window() {
        let mut state = state_with(rows(100));
        state.viewport = 30;

        // A selection has to exist first: with none, a jump merely lands on row 0
        // rather than moving by its delta.
        dispatch(&mut state, Action::Down);
        assert_eq!(cursor_of(&state), 0);

        dispatch(&mut state, Action::PageDown);
        assert_eq!(cursor_of(&state), 15, "half of a thirty-row window");
    }

    #[test]
    fn scroll_to_keep_moves_only_when_the_cursor_reaches_a_margin() {
        let (rows, viewport) = (100, 10);

        let mut scroll = 0;
        for cursor in 0..100 {
            scroll = scroll_to_keep(scroll, cursor, rows, viewport);
            assert!(
                cursor >= scroll && cursor < scroll + viewport,
                "cursor {cursor} lost at scroll {scroll}"
            );
        }
        assert_eq!(scroll, rows - viewport, "the window pins at the end");
    }

    #[test]
    fn scroll_to_keep_restores_context_when_the_cursor_jumps_up() {
        let scroll = scroll_to_keep(80, 50, 100, 10);
        assert!(
            scroll <= 50,
            "cursor 50 shown with context: scroll {scroll}"
        );
        assert!(50 < scroll + 10, "cursor 50 inside the window: {scroll}");

        assert_eq!(scroll_to_keep(50, 0, 100, 10), 0, "top is pinned");
    }

    #[test]
    fn scroll_to_keep_resolves_degenerate_windows() {
        assert_eq!(scroll_to_keep(9, 0, 0, 10), 0, "no rows");
        assert_eq!(scroll_to_keep(9, 5, 5, 10), 0, "everything fits");
        assert_eq!(scroll_to_keep(9, 5, 100, 0), 0, "no viewport");
        assert_eq!(scroll_to_keep(9, 5, 100, 1), 5, "one-row window");
    }

    #[test]
    fn popup_actions_set_the_mode() {
        for (action, expected) in [
            (Action::Help, Popup::Help),
            (Action::SortMenu, Popup::Sort),
            (Action::FilterMenu, Popup::Filter),
            (Action::SkinMenu, Popup::Skin),
            (Action::LogMenu, Popup::Log),
        ] {
            let mut state = state_with(rows(1));
            dispatch(&mut state, action);
            assert_eq!(state.mode.popup(), Some(expected), "{action:?}");
        }
    }

    /// Opening on the skin in force is what makes the list read as "you are here", and
    /// it is also what makes Esc from an untouched picker a no-op.
    #[test]
    fn the_skin_picker_opens_on_the_skin_in_force() {
        let mut state = state_with(rows(1));
        state.theme = state.theme.with_skin("nord");

        dispatch(&mut state, Action::SkinMenu);

        let popup = state.mode.popup_state().unwrap();
        assert_eq!(skins::BUILTIN_NAMES[popup.cursor], "nord");
        assert_eq!(popup.previous_skin.as_deref(), Some("nord"));
    }

    /// The picker previews: a list of names says nothing about what the table will look
    /// like, so moving the cursor has to repaint the frame behind it.
    #[test]
    fn moving_the_picker_cursor_previews_the_skin() {
        let mut state = state_with(rows(1));
        let keymap = keymap();
        dispatch(&mut state, Action::SkinMenu);

        handle_key(&mut state, &keymap, key(KeyCode::Char('j')));

        let cursor = state.mode.popup_state().unwrap().cursor;
        assert_eq!(state.theme.skin(), skins::BUILTIN_NAMES[cursor]);
        assert_ne!(state.theme.skin(), skins::BUILTIN_NAMES[0]);
    }

    #[test]
    fn enter_keeps_the_previewed_skin_and_says_which() {
        let mut state = state_with(rows(1));
        let keymap = keymap();
        dispatch(&mut state, Action::SkinMenu);

        handle_key(&mut state, &keymap, key(KeyCode::Char('j')));
        let previewed = state.theme.skin().to_owned();
        handle_key(&mut state, &keymap, key(KeyCode::Enter));

        assert!(state.mode.is_normal());
        assert_eq!(state.theme.skin(), previewed);
        assert_eq!(
            state.flash.as_ref().map(|f| f.message.as_str()),
            Some(format!("skin: {previewed}").as_str())
        );
    }

    /// Esc has to undo the preview, or cancelling leaves the user in a skin they never
    /// chose and cannot name.
    #[test]
    fn esc_restores_the_skin_the_picker_opened_with() {
        let mut state = state_with(rows(1));
        let keymap = keymap();
        state.theme = state.theme.with_skin("gruvbox-dark");

        dispatch(&mut state, Action::SkinMenu);
        for _ in 0..3 {
            handle_key(&mut state, &keymap, key(KeyCode::Char('j')));
        }
        assert_ne!(state.theme.skin(), "gruvbox-dark", "it previewed");

        handle_key(&mut state, &keymap, key(KeyCode::Esc));

        assert!(state.mode.is_normal());
        assert_eq!(state.theme.skin(), "gruvbox-dark");
    }

    /// A skin's initial letter jumps to it, as a column's does in the sort menu.
    #[test]
    fn a_skins_initial_letter_jumps_to_it() {
        let mut state = state_with(rows(1));
        let keymap = keymap();
        dispatch(&mut state, Action::SkinMenu);

        handle_key(&mut state, &keymap, key(KeyCode::Char('n')));

        assert_eq!(state.theme.skin(), "nord");
    }

    /// The cursor is bounded by the list, and the preview follows it to the end rather
    /// than running off into a skin that does not exist.
    #[test]
    fn the_picker_cursor_stops_at_the_last_skin() {
        let mut state = state_with(rows(1));
        let keymap = keymap();
        dispatch(&mut state, Action::SkinMenu);

        for _ in 0..100 {
            handle_key(&mut state, &keymap, key(KeyCode::Char('j')));
        }

        let last = skins::BUILTIN_NAMES.len() - 1;
        assert_eq!(state.mode.popup_state().unwrap().cursor, last);
        assert_eq!(state.theme.skin(), skins::BUILTIN_NAMES[last]);
    }

    /// While typing a search, `d` is a letter, not the draft toggle. Getting this wrong
    /// makes the search box unusable in a way that looks like random misbehaviour.
    #[test]
    fn search_mode_captures_printable_keys() {
        let mut state = state_with(rows(3));
        let keymap = keymap();

        dispatch(&mut state, Action::Search);
        let drafts_before = state.tabs.active().unwrap().show_drafts();

        for c in "dqs".chars() {
            handle_key(&mut state, &keymap, key(KeyCode::Char(c)));
        }

        assert_eq!(
            state.mode,
            Mode::Search {
                query: "dqs".into()
            }
        );
        assert_eq!(
            state.tabs.active().unwrap().show_drafts(),
            drafts_before,
            "`d` typed a letter, it did not toggle drafts"
        );
    }

    #[test]
    fn search_filters_the_visible_rows_as_you_type() {
        let mut state = state_with(rows(5));
        let keymap = keymap();

        dispatch(&mut state, Action::Search);
        for c in "request 3".chars() {
            handle_key(&mut state, &keymap, key(KeyCode::Char(c)));
        }

        let visible = state.visible_rows();
        assert_eq!(
            visible.len(),
            1,
            "{:?}",
            visible.iter().map(|m| &m.title).collect::<Vec<_>>()
        );
        assert_eq!(visible[0].title, "merge request 3");
    }

    #[test]
    fn backspace_shortens_the_query() {
        let mut state = state_with(rows(5));
        let keymap = keymap();

        dispatch(&mut state, Action::Search);
        for c in "abc".chars() {
            handle_key(&mut state, &keymap, key(KeyCode::Char(c)));
        }
        handle_key(&mut state, &keymap, key(KeyCode::Backspace));

        assert_eq!(state.mode, Mode::Search { query: "ab".into() });
    }

    #[test]
    fn enter_commits_the_search_and_escape_clears_it() {
        let mut state = state_with(rows(5));
        let keymap = keymap();

        dispatch(&mut state, Action::Search);
        for c in "request 1".chars() {
            handle_key(&mut state, &keymap, key(KeyCode::Char(c)));
        }
        handle_key(&mut state, &keymap, key(KeyCode::Enter));

        assert!(state.mode.is_normal());
        assert_eq!(
            state.tabs.active().unwrap().search.as_deref(),
            Some("request 1")
        );

        handle_key(&mut state, &keymap, key(KeyCode::Esc));
        assert_eq!(state.tabs.active().unwrap().search, None);
    }

    /// A modal bug that traps the user in the search box is worse than a broken search.
    #[test]
    fn ctrl_c_quits_from_inside_the_search_box() {
        let mut state = state_with(rows(3));
        let keymap = keymap();

        dispatch(&mut state, Action::Search);
        let outcome = handle_key(
            &mut state,
            &keymap,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert!(
            matches!(
                outcome,
                KeyOutcome::Action {
                    effect: Effect::Quit,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    #[test]
    fn escape_closes_a_popup_and_quit_still_works_inside_one() {
        let keymap = keymap();

        let mut state = state_with(rows(1));
        dispatch(&mut state, Action::Help);
        handle_key(&mut state, &keymap, key(KeyCode::Esc));
        assert!(state.mode.is_normal());

        dispatch(&mut state, Action::Help);
        let outcome = handle_key(&mut state, &keymap, key(KeyCode::Char('q')));
        assert!(
            matches!(
                outcome,
                KeyOutcome::Action {
                    effect: Effect::Quit,
                    ..
                }
            ),
            "a modal bug must not trap the user: {outcome:?}"
        );
    }

    #[test]
    fn the_key_that_opened_a_popup_closes_it_again() {
        let keymap = keymap();
        let mut state = state_with(rows(1));

        dispatch(&mut state, Action::Help);
        assert!(state.mode.popup().is_some());

        // "?" is Help's own key, not Esc.
        handle_key(&mut state, &keymap, key(KeyCode::Char('?')));
        assert!(state.mode.is_normal());
    }

    /// Filter's key doubles as ordinary query text, so it must keep typing rather than
    /// close the popup on every repeat of the letter that opened it.
    #[test]
    fn filters_own_key_types_into_the_query_instead_of_closing_it() {
        let keymap = keymap();
        let mut state = state_with(rows(1));

        dispatch(&mut state, Action::FilterMenu);
        handle_key(&mut state, &keymap, key(KeyCode::Char('f')));

        assert!(state.mode.popup().is_some(), "still open");
        assert_eq!(state.mode.popup_state().unwrap().query, "f");
    }

    /// A popup swallows keys so the table underneath does not also act on them.
    ///
    /// `Handled`, not `Ignored`: the key was consumed by the popup even though it did
    /// nothing there. Reporting it as ignored would invite a caller to fall through to
    /// the table, which is the bug the capture exists to prevent.
    #[test]
    fn a_popup_captures_other_keys() {
        let mut state = state_with(rows(3));
        let keymap = keymap();

        dispatch(&mut state, Action::Help);
        let before = state.tabs.active().unwrap().show_drafts();
        let outcome = handle_key(&mut state, &keymap, key(KeyCode::Char('d')));

        assert_eq!(outcome, KeyOutcome::Handled { redraw: false });
        assert_eq!(state.tabs.active().unwrap().show_drafts(), before);
        assert!(state.mode.popup().is_some(), "and the popup stayed open");
    }

    /// Logging per keystroke would fill the 50-line popup the moment someone leans on
    /// the keyboard.
    #[test]
    fn unbound_keys_are_ignored_silently() {
        let mut state = state_with(rows(3));
        let keymap = keymap();

        let outcome = handle_key(&mut state, &keymap, key(KeyCode::Char('~')));
        assert_eq!(outcome, KeyOutcome::Ignored);
    }

    /// Leaving a message up after the user has moved on makes it look like a persistent
    /// condition rather than a one-off.
    #[test]
    fn the_next_action_clears_a_transient_message() {
        let mut state = state_with(rows(1));
        dispatch(&mut state, Action::Down);
        dispatch(&mut state, Action::OpenPipeline);
        assert!(state.flash.is_some());

        dispatch(&mut state, Action::Down);
        assert!(state.flash.is_none());
    }

    #[test]
    fn search_reopens_with_the_committed_query() {
        let mut state = state_with(rows(5));
        let keymap = keymap();

        dispatch(&mut state, Action::Search);
        for c in "req".chars() {
            handle_key(&mut state, &keymap, key(KeyCode::Char(c)));
        }
        handle_key(&mut state, &keymap, key(KeyCode::Enter));

        dispatch(&mut state, Action::Search);
        assert_eq!(
            state.mode,
            Mode::Search {
                query: "req".into()
            }
        );
    }

    #[test]
    fn drafts_are_hidden_unless_the_tab_shows_them() {
        let mut list = rows(3);
        list[1].draft = true;
        let mut state = state_with(list);

        if let Some(tab) = state.tabs.active_mut()
            && tab.show_drafts()
        {
            tab.toggle_drafts();
        }
        assert_eq!(state.visible_rows().len(), 2);

        dispatch(&mut state, Action::ToggleDrafts);
        assert_eq!(state.visible_rows().len(), 3);
    }

    #[test]
    fn search_matches_title_author_and_repo_only() {
        let mut m = mr("1", "asmith");
        m.title = "Add dark mode".into();
        m.project_name = "web-app".into();
        m.source_branch = "feat/redis".into();

        assert!(matches_search(&m, "dark"));
        assert!(matches_search(&m, "ASMITH"));
        assert!(matches_search(&m, "web"));
        assert!(!matches_search(&m, "redis"), "branches are not searched");
        assert!(matches_search(&m, ""));
    }
}

/// The performance budget for cursor movement, pinned against regression.
///
/// Separate from the unit tests because it is the only one whose failure means "this got
/// slower", not "this got wrong". It lives here rather than in a benchmark harness so it
/// runs in CI on every commit, which is where a regression would otherwise go unnoticed
/// until someone opened the TUI on a large instance.
#[cfg(test)]
mod perf {
    use super::*;
    use crate::config::schema::{Filter, Scope, Sort};
    use crate::gitlab::model::fixtures::mr;
    use std::time::Instant as Clock;

    /// "Sort or search over 500 rows: < 5 ms, i.e. within one frame."
    const BUDGET: std::time::Duration = std::time::Duration::from_millis(5);

    /// The idle-CPU scenario, one filter wider than its "10 filters x 100 MRs" memory case so
    /// the sort target is exercised too.
    const FILTERS: usize = 10;
    const ROWS: usize = 500;

    fn loaded_state(filters: usize, rows: usize) -> ViewState {
        let fs: Vec<Filter> = (0..filters)
            .map(|i| Filter::named(&format!("F{i}"), Scope::Assigned))
            .collect();
        let mut tabs = Tabs::new(&fs, Sort::default(), true, None);

        for tab in 0..filters {
            let rows: Vec<MergeRequest> = (0..rows)
                .map(|i| {
                    let mut m = mr(&format!("gid://{tab}-{i}"), "someone");
                    m.title = format!("merge request number {i} with a reasonably long title");
                    m
                })
                .collect();
            tabs.get_mut(tab).unwrap().apply_rows(rows, Instant::now());
        }

        ViewState {
            tabs,
            mode: Mode::Normal,
            wide: false,
            theme: test_theme(),
            drafts_last: true,
            flash: None,
            log: LogBuffer::new(),
            viewport: HALF_PAGE_VIEWPORT,
        }
    }

    /// Everything one keypress costs: the dispatch, then what `App::draw` gathers.
    ///
    /// The state is mutated every iteration on purpose. With it loop-invariant the
    /// optimiser hoists the whole body out and the test measures nothing — an earlier
    /// version of this probe reported 30ns for filtering 5000 rows.
    fn keystroke(state: &mut ViewState) {
        dispatch(state, Action::Down);

        let rows = state.visible_rows();
        let counts: Vec<usize> = (0..state.tabs.len()).map(|i| state.count_of(i)).collect();
        let selected = state
            .tabs
            .active()
            .and_then(|tab| tab.selected_id())
            .and_then(|id| rows.iter().position(|mr| mr.id == id));

        std::hint::black_box((rows, counts, selected));
    }

    #[test]
    fn a_keystroke_stays_within_the_frame_budget() {
        let mut state = loaded_state(FILTERS, ROWS);
        // One pass to warm the allocator and settle the selection.
        keystroke(&mut state);

        const ITERATIONS: u32 = 20;
        let start = Clock::now();
        for _ in 0..ITERATIONS {
            keystroke(&mut state);
        }
        let each = start.elapsed() / ITERATIONS;

        // Debug builds are several times slower than the release binary users run, and
        // CI runners are slower again, so the assertion is generous. It is a tripwire for
        // a change that reintroduces per-frame cloning of every tab, not a benchmark:
        // that regression was 8.3ms here in debug, against the 0.8ms this now costs.
        let ceiling = BUDGET * 4;
        assert!(
            each < ceiling,
            "a keystroke over {FILTERS} filters x {ROWS} rows took {each:?}, \
             over the {ceiling:?} tripwire ({BUDGET:?} budgeted in release)"
        );
    }

    /// Prints the cost at several sizes, for mrq-ae9 and for re-deriving the numbers in
    /// a commit message. Ignored: it reports, it does not assert.
    ///
    /// `cargo test --release -- --ignored --nocapture print_keystroke_cost`
    #[test]
    #[ignore = "reports keystroke cost for re-deriving numbers in a commit message"]
    fn print_keystroke_cost() {
        for (filters, rows) in [(1, 100), (10, 100), (FILTERS, ROWS)] {
            let mut state = loaded_state(filters, rows);
            keystroke(&mut state);

            const ITERATIONS: u32 = 100;
            let start = Clock::now();
            for _ in 0..ITERATIONS {
                keystroke(&mut state);
            }
            println!(
                "{filters} filters x {rows} rows: {:?} per keystroke",
                start.elapsed() / ITERATIONS
            );
        }
    }

    /// The count must not depend on cloning or sorting, which is what made it expensive.
    #[test]
    fn counting_agrees_with_listing() {
        let mut state = loaded_state(FILTERS, ROWS);
        for index in 0..state.tabs.len() {
            assert_eq!(state.count_of(index), state.rows_of(index).len());
        }

        // And with the filters applied, where the two could most easily diverge.
        state.tabs.get_mut(0).unwrap().search = Some("number 1".to_owned());
        state.tabs.get_mut(0).unwrap().toggle_drafts();
        assert_eq!(state.count_of(0), state.rows_of(0).len());
    }
}
