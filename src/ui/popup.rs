//! The overlays: help, sort menu, filter switcher, skin picker, and the log.
//!
//! # The help popup is generated
//!
//! It is built from the resolved keymap, never from a list written here. A hardcoded
//! help screen is correct exactly until someone rebinds a key, and then it is a
//! confidently wrong answer to the one question it exists to answer. Every line in it —
//! the key, the description, the grouping — comes from the same tables the dispatcher
//! uses, so a new action appears in the help the moment it is bound.
//!
//! # Scrolling, not clipping
//!
//! A popup taller than the frame is scrolled to the cursor rather than cut off. A help
//! screen that silently loses its last category on a short terminal is worse than no
//! help screen: the user has no way to know there was more.

use jiff::Timestamp;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::action::{
    self, HelpLine, Popup, PopupState, SORTABLE, ViewState, matching_filters,
};
use crate::config::keymap::Keymap;
use crate::ui::skins;
use crate::ui::table::truncate;
use crate::ui::theme::{Role, Theme};

/// Share of the body a popup covers.
const WIDTH_PCT: u8 = 70;
const HEIGHT_PCT: u8 = 70;

/// Centre a popup over the body, clamped so it always fits.
pub fn area(body: Rect) -> Rect {
    crate::ui::layout::centred(body, WIDTH_PCT, HEIGHT_PCT)
}

/// The title each popup carries, so the user knows what they opened.
const fn title(kind: Popup) -> &'static str {
    match kind {
        Popup::Help => " Keys ",
        Popup::Sort => " Sort by ",
        Popup::Filter => " Filters ",
        Popup::Skin => " Skin ",
        Popup::Log => " Log ",
        Popup::Details => " Details ",
    }
}

/// Build the open popup's widget, and the `Clear` that has to be drawn under it.
///
/// Returns `None` when no popup is open, so the caller has one branch rather than five.
pub fn build<'a>(
    state: &ViewState,
    keymap: &Keymap,
    theme: &Theme,
    now: Timestamp,
    area: Rect,
) -> Option<(Clear, Paragraph<'a>)> {
    let popup = state.mode.popup_state()?;

    // Two cells of border and one of padding on each side.
    let inner_width = usize::from(area.width).saturating_sub(4);
    let inner_height = usize::from(area.height).saturating_sub(2);

    let lines = match popup.kind {
        Popup::Help => help_lines(keymap, theme, inner_width),
        Popup::Sort => sort_lines(state, popup, theme, inner_width),
        Popup::Filter => filter_lines(state, popup, theme, now, inner_width),
        Popup::Skin => skin_lines(popup, theme, inner_width),
        Popup::Log => log_lines(popup, theme, inner_width),
        Popup::Details => details_lines(popup, theme, inner_width),
    };

    let block = theme
        .panel(title(popup.kind))
        .padding(Padding::horizontal(1));
    let scrolled = scroll_to(lines, popup.cursor, inner_height);
    Some((Clear, Paragraph::new(scrolled).block(block)))
}

/// Take the window of `lines` that keeps `cursor` visible.
///
/// There are `lines.len()` cursor positions but only `last_start + 1` places the window
/// can start from, so some positions are inevitably going to share a window — the
/// question is only where that slack lands. Truncating (`cursor.min(last_start)`) dumps
/// all of it on the last page: from the bottom, `k` would sit dead for up to
/// `height - 1` presses before the window actually moved, which reads as a stuck popup
/// rather than as scrolling up. Scaling `cursor` onto `0..=last_start` instead spreads
/// that same slack evenly across the whole range, so every direction responds within a
/// press or two — matching what already happens scrolling down from the top, where
/// `cursor` and `start` agree exactly because there is no slack down there yet.
///
/// Clipping instead of windowing would silently lose the end of a long help screen, and
/// the user has no way to know there was more.
fn scroll_to<'a>(lines: Vec<Line<'a>>, cursor: usize, height: usize) -> Vec<Line<'a>> {
    if height == 0 || lines.len() <= height {
        return lines;
    }

    let last_start = lines.len() - height;
    let last_cursor = lines.len() - 1;
    // Rounded rather than truncated, so the slack is distributed rather than always
    // favouring the lower `start` (which would bias every repeat towards not scrolling).
    let start = (cursor * last_start + last_cursor / 2) / last_cursor;
    lines[start..start + height].to_vec()
}

fn row<'a>(text: String, role: Role, theme: &Theme, selected: bool, width: usize) -> Line<'a> {
    let mut text = truncate(&text, width, theme.ellipsis());
    if selected {
        // Padded to the full width so the highlight is a bar across the popup rather
        // than a badge around whatever that entry happened to be called.
        text.push_str(&" ".repeat(width.saturating_sub(text.width())));
        return Line::from(Span::styled(text, theme.style(Role::Selection)));
    }
    Line::from(Span::styled(text, theme.style(role)))
}

/// The help popup, rendered from the structure `action::help_lines` generated.
fn help_lines<'a>(keymap: &Keymap, theme: &Theme, width: usize) -> Vec<Line<'a>> {
    let entries = action::help_lines(keymap);
    if entries.is_empty() {
        return vec![row(
            "every key is unbound".to_owned(),
            Role::Dim,
            theme,
            false,
            width,
        )];
    }

    // Measured rather than fixed: this is generated from the *resolved* keymap, so the
    // widest key list is whatever the user bound, and a constant is a width someone's
    // binding will exceed — pushing that row's description out of the column.
    let key_column = entries
        .iter()
        .filter_map(|entry| match entry {
            HelpLine::Binding { keys, .. } => Some(keys.width()),
            HelpLine::Blank | HelpLine::Heading(_) => None,
        })
        .max()
        .unwrap_or(0);

    entries
        .into_iter()
        .map(|entry| match entry {
            HelpLine::Blank => Line::default(),
            HelpLine::Heading(title) => row(title.to_owned(), Role::Header, theme, false, width),
            HelpLine::Binding { keys, description } => {
                let padding = " ".repeat(key_column.saturating_sub(keys.width()));
                row(
                    format!("  {keys}{padding} {description}"),
                    Role::Normal,
                    theme,
                    false,
                    width,
                )
            }
        })
        .collect()
}

/// The sort menu: every sortable column, the current one marked.
fn sort_lines<'a>(
    state: &ViewState,
    popup: &PopupState,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'a>> {
    let current = state.tabs.active().map(|tab| tab.sort_column);
    let order = state.tabs.active().map(|tab| tab.sort_order);

    SORTABLE
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let active = Some(*column) == current;
            let marker = if active {
                format!(
                    " {}",
                    theme.sort_arrow(order == Some(crate::config::schema::Order::Asc))
                )
            } else {
                String::new()
            };
            row(
                format!(" {}{marker}", column.header()),
                Role::Normal,
                theme,
                index == popup.cursor,
                width,
            )
        })
        .collect()
}

/// The filter switcher: every matching filter with its count and last-refresh age.
fn filter_lines<'a>(
    state: &ViewState,
    popup: &PopupState,
    theme: &Theme,
    now: Timestamp,
    width: usize,
) -> Vec<Line<'a>> {
    let matches = matching_filters(state, &popup.query);
    let mut lines = vec![row(
        format!("/{}", popup.query),
        Role::Accent,
        theme,
        false,
        width,
    )];

    if matches.is_empty() {
        lines.push(row(
            " no filter matches".to_owned(),
            Role::Dim,
            theme,
            false,
            width,
        ));
        return lines;
    }

    for (position, index) in matches.iter().copied().enumerate() {
        let Some(tab) = state.tabs.get(index) else {
            continue;
        };
        let age = match tab.fetched_at {
            Some(_) => refreshed_label(state, index, now),
            None => "never refreshed".to_owned(),
        };
        lines.push(row(
            format!(
                " {}{:<20} {:>4}  {age}",
                shortcut(index),
                tab.name,
                state.rows_of(index).len()
            ),
            Role::Normal,
            theme,
            position == popup.cursor,
            width,
        ));
    }
    lines
}

fn shortcut(index: usize) -> String {
    match index {
        0..9 => format!("{}:", index + 1),
        _ => "  ".to_owned(),
    }
}

/// How long ago a filter last refreshed.
///
/// Derived from the newest merge request rather than the fetch instant: `Instant` has no
/// relation to the wall clock the rest of the popup uses, and the user reads this
/// alongside timestamps.
fn refreshed_label(state: &ViewState, index: usize, now: Timestamp) -> String {
    match state.rows_of(index).iter().map(|mr| mr.updated_at).max() {
        Some(latest) => format!(
            "newest {} ago",
            crate::ui::table::relative_time(latest, now)
        ),
        None => "empty".to_owned(),
    }
}

/// The skin picker: every built-in, the one being previewed marked.
///
/// No swatch preview strip: the frame behind the popup is already drawn in the skin under
/// the cursor, which is the only preview that answers "what will my table look like".
fn skin_lines<'a>(popup: &PopupState, theme: &Theme, width: usize) -> Vec<Line<'a>> {
    skins::BUILTIN_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            row(
                format!(" {name}"),
                Role::Normal,
                theme,
                index == popup.cursor,
                width,
            )
        })
        .collect()
}

/// The log popup: the ring buffer as it was when the popup opened.
fn log_lines<'a>(popup: &PopupState, theme: &Theme, width: usize) -> Vec<Line<'a>> {
    if popup.lines.is_empty() {
        return vec![row(
            "nothing logged yet".to_owned(),
            Role::Dim,
            theme,
            false,
            width,
        )];
    }

    popup
        .lines
        .iter()
        .map(|line| row(line.clone(), Role::Normal, theme, false, width))
        .collect()
}

/// The merge request details popup: header fields and the markdown-rendered description,
/// exactly as `action::detail_lines` built them when the popup opened.
fn details_lines<'a>(popup: &PopupState, theme: &Theme, width: usize) -> Vec<Line<'a>> {
    popup
        .styled
        .iter()
        .map(|line| styled_row(line, theme, width))
        .collect()
}

/// One rendered markdown line, truncated across its segments so the popup never wraps
/// past `width` regardless of how many styled runs a line was split into.
fn styled_row<'a>(line: &[crate::ui::markdown::Segment], theme: &Theme, width: usize) -> Line<'a> {
    let mut spans = Vec::new();
    let mut used = 0usize;

    for segment in line {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let text = truncate(&segment.text, remaining, theme.ellipsis());
        used += text.width();
        spans.push(Span::styled(text, theme.style(segment.role)));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::action::{HALF_PAGE_VIEWPORT, Mode, dispatch};
    use crate::app::state::Tabs;
    use crate::config::keymap::{Action, Category};
    use crate::config::schema::{Column, Filter, Scope, Sort};
    use crate::gitlab::model::fixtures::mr;
    use crate::term::caps::{Capabilities, ColorDepth, NotifyEscape};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::BTreeMap;
    use std::time::Instant;

    fn theme() -> Theme {
        let caps = Capabilities {
            color: ColorDepth::TrueColor,
            hyperlinks: false,
            notify: NotifyEscape::None,
            focus_events: true,
            multiplexed: false,
            over_ssh: false,
        };
        Theme::builtin("catppuccin-mocha", false, &caps)
    }

    fn keymap() -> Keymap {
        crate::config::keymap::resolve(&BTreeMap::new()).unwrap()
    }

    fn now() -> Timestamp {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn state() -> ViewState {
        let mut tabs = Tabs::new(
            &[
                Filter::named("Assigned", Scope::Assigned),
                Filter::named("Platform", Scope::Group),
                Filter::named("Reviewing", Scope::ReviewRequested),
            ],
            Sort::default(),
            true,
            None,
        );
        tabs.get_mut(0)
            .unwrap()
            .apply_rows(vec![mr("a", "x"), mr("b", "x")], Instant::now());

        ViewState {
            tabs,
            mode: Mode::Normal,
            wide: false,
            theme: theme(),
            drafts_last: false,
            flash: None,
            log: crate::logging::LogBuffer::new(),
            viewport: HALF_PAGE_VIEWPORT,
        }
    }

    fn open(state: &mut ViewState, action: Action) {
        dispatch(state, action);
    }

    fn press(state: &mut ViewState, code: KeyCode) {
        crate::app::action::handle_key(state, &keymap(), KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn press_ctrl(state: &mut ViewState, code: KeyCode) {
        crate::app::action::handle_key(
            state,
            &keymap(),
            KeyEvent::new(code, KeyModifiers::CONTROL),
        );
    }

    fn rendered(state: &ViewState, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let region = area(frame.area());
                if let Some((clear, widget)) = build(state, &keymap(), &theme(), now(), region) {
                    frame.render_widget(clear, region);
                    frame.render_widget(widget, region);
                }
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    fn text(state: &ViewState) -> String {
        rendered(state, 100, 30).join("\n")
    }

    /// The key column is as wide as the widest key list, because it is generated from
    /// the user's keymap: a fixed width is a width someone's binding will exceed, and the
    /// shipped defaults already do.
    #[test]
    fn every_help_description_starts_in_the_same_column() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "quit".to_owned(),
            vec!["f10".to_owned(), "f11".to_owned(), "f12".to_owned()],
        );
        let wide = crate::config::keymap::resolve(&bindings).unwrap();

        for keymap in [keymap(), wide] {
            let descriptions: Vec<usize> = action::help_lines(&keymap)
                .iter()
                .zip(help_lines(&keymap, &theme(), 200))
                .filter_map(|(entry, line)| match entry {
                    HelpLine::Binding { description, .. } => {
                        let text = line.to_string();
                        Some(
                            text.find(description)
                                .unwrap_or_else(|| panic!("`{description}` missing from `{text}`")),
                        )
                    }
                    HelpLine::Blank | HelpLine::Heading(_) => None,
                })
                .collect();

            let first = descriptions[0];
            assert!(
                descriptions.iter().all(|start| *start == first),
                "descriptions start at {descriptions:?}"
            );
        }
    }

    /// Generated from the resolved keymap, so rebinding a key changes the
    /// help rather than making it lie.
    #[test]
    fn the_help_popup_reflects_a_rebound_key() {
        let mut bindings = BTreeMap::new();
        bindings.insert("quit".to_owned(), vec!["ctrl-x".to_owned()]);
        let rebound = crate::config::keymap::resolve(&bindings).unwrap();

        let lines = help_lines(&rebound, &theme(), 80);
        let joined: String = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(joined.contains("ctrl-x"), "{joined}");
        assert!(
            joined.contains(Action::Quit.description()),
            "the description comes from the action table: {joined}"
        );
    }

    #[test]
    fn the_help_popup_is_grouped_by_category() {
        let joined: String = help_lines(&keymap(), &theme(), 80)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        for category in [
            Category::Navigation,
            Category::Actions,
            Category::Application,
        ] {
            assert!(joined.contains(category.title()), "{joined}");
        }
    }

    /// Opening on the column in force means Enter without moving is a no-op rather than
    /// a silent re-sort.
    #[test]
    fn the_sort_menu_opens_on_the_current_column() {
        let mut state = state();
        state.tabs.active_mut().unwrap().sort_column = Column::Author;
        open(&mut state, Action::SortMenu);

        let cursor = state.mode.popup_state().unwrap().cursor;
        assert_eq!(SORTABLE[cursor], Column::Author);
    }

    #[test]
    fn the_sort_menu_marks_the_current_column() {
        let mut state = state();
        open(&mut state, Action::SortMenu);
        let shown = text(&state);

        assert!(shown.contains("UPDATED"), "{shown}");
        assert!(shown.contains(theme().sort_arrow(false)), "{shown}");
    }

    #[test]
    fn j_and_k_move_the_sort_cursor_and_enter_applies_it() {
        let mut state = state();
        open(&mut state, Action::SortMenu);
        let start = state.mode.popup_state().unwrap().cursor;

        press(&mut state, KeyCode::Char('j'));
        assert_eq!(state.mode.popup_state().unwrap().cursor, start + 1);
        press(&mut state, KeyCode::Char('k'));
        assert_eq!(state.mode.popup_state().unwrap().cursor, start);

        press(&mut state, KeyCode::Char('j'));
        press(&mut state, KeyCode::Enter);

        assert!(state.mode.is_normal(), "Enter closes the popup");
        assert_eq!(
            state.tabs.active().unwrap().sort_column,
            SORTABLE[start + 1]
        );
    }

    /// A column's initial letter jumps to it.
    #[test]
    fn a_columns_initial_letter_jumps_to_it() {
        let mut state = state();
        open(&mut state, Action::SortMenu);

        press(&mut state, KeyCode::Char('a'));
        let cursor = state.mode.popup_state().unwrap().cursor;
        assert!(
            SORTABLE[cursor].header().to_lowercase().starts_with('a'),
            "{:?}",
            SORTABLE[cursor]
        );
    }

    #[test]
    fn esc_cancels_the_sort_without_applying_it() {
        let mut state = state();
        let before = state.tabs.active().unwrap().sort_column;
        open(&mut state, Action::SortMenu);

        press(&mut state, KeyCode::Char('j'));
        press(&mut state, KeyCode::Esc);

        assert!(state.mode.is_normal());
        assert_eq!(state.tabs.active().unwrap().sort_column, before);
    }

    #[test]
    fn the_filter_switcher_shows_counts_and_ages() {
        let mut state = state();
        open(&mut state, Action::FilterMenu);
        let shown = text(&state);

        assert!(shown.contains("Assigned"), "{shown}");
        assert!(shown.contains("Platform"), "{shown}");
        assert!(shown.contains('2'), "the count of rows: {shown}");
        assert!(shown.contains("never refreshed"), "{shown}");
    }

    /// `plt` finds `Platform`: a subsequence, not a prefix.
    #[test]
    fn the_filter_switcher_matches_a_subsequence() {
        let mut state = state();
        open(&mut state, Action::FilterMenu);

        for c in "plt".chars() {
            press(&mut state, KeyCode::Char(c));
        }

        let matches = matching_filters(&state, &state.mode.popup_state().unwrap().query);
        assert_eq!(matches.len(), 1);
        assert_eq!(state.tabs.get(matches[0]).unwrap().name, "Platform");
    }

    #[test]
    fn enter_activates_the_matched_filter() {
        let mut state = state();
        open(&mut state, Action::FilterMenu);
        for c in "rev".chars() {
            press(&mut state, KeyCode::Char(c));
        }
        press(&mut state, KeyCode::Enter);

        assert!(state.mode.is_normal());
        assert_eq!(state.tabs.active().unwrap().name, "Reviewing");
    }

    #[test]
    fn backspace_widens_the_filter_query_again() {
        let mut state = state();
        open(&mut state, Action::FilterMenu);
        for c in "plt".chars() {
            press(&mut state, KeyCode::Char(c));
        }
        press(&mut state, KeyCode::Backspace);

        assert_eq!(state.mode.popup_state().unwrap().query, "pl");
    }

    #[test]
    fn a_query_matching_nothing_says_so() {
        let mut state = state();
        open(&mut state, Action::FilterMenu);
        for c in "zzzz".chars() {
            press(&mut state, KeyCode::Char(c));
        }

        assert!(text(&state).contains("no filter matches"));
    }

    #[test]
    fn the_skin_picker_lists_every_built_in() {
        let mut state = state();
        open(&mut state, Action::SkinMenu);
        let shown = text(&state);

        for name in crate::ui::skins::BUILTIN_NAMES {
            assert!(shown.contains(name), "{name} is missing from: {shown}");
        }
    }

    #[test]
    fn the_log_popup_shows_the_buffered_lines() {
        let mut state = state();
        open(&mut state, Action::LogMenu);
        assert!(text(&state).contains("nothing logged yet"));
    }

    #[test]
    fn the_details_popup_shows_the_selected_merge_request() {
        let mut state = state();
        state
            .tabs
            .active_mut()
            .unwrap()
            .select(Some("a".to_owned()));
        open(&mut state, Action::ShowDetails);

        let shown = text(&state);
        assert!(shown.contains("web-app"), "{shown}");
        assert!(shown.contains("482"), "{shown}");
        assert!(shown.contains("Add dark mode toggle"), "{shown}");
        assert!(shown.contains("Description:"), "{shown}");
    }

    /// The log popup shows the last 50 lines, which is the ring's capacity.
    #[test]
    fn the_log_popup_snapshots_the_ring_when_it_opens() {
        let mut state = state();
        state.log = crate::logging::LogBuffer::new();
        open(&mut state, Action::LogMenu);

        let captured = state.mode.popup_state().unwrap().lines.clone();
        assert_eq!(captured, state.log.lines());
    }

    /// Popups capture input, or `j` scrolls a table the user cannot see.
    #[test]
    fn a_popup_captures_keys_that_would_otherwise_act() {
        let mut state = state();
        state
            .tabs
            .active_mut()
            .unwrap()
            .select(Some("a".to_owned()));
        open(&mut state, Action::Help);

        // `d` toggles drafts in normal mode.
        let drafts = state.tabs.active().unwrap().show_drafts();
        press(&mut state, KeyCode::Char('d'));

        assert_eq!(state.tabs.active().unwrap().show_drafts(), drafts);
        assert!(state.mode.popup().is_some(), "and the popup stayed open");
    }

    #[test]
    fn esc_closes_every_popup() {
        for action in [
            Action::Help,
            Action::SortMenu,
            Action::FilterMenu,
            Action::SkinMenu,
            Action::LogMenu,
            Action::ShowDetails,
        ] {
            let mut state = state();
            state
                .tabs
                .active_mut()
                .unwrap()
                .select(Some("a".to_owned()));
            open(&mut state, action);
            assert!(state.mode.popup().is_some());

            press(&mut state, KeyCode::Esc);
            assert!(state.mode.is_normal(), "{action:?} did not close");
        }
    }

    /// A help screen that silently loses its last category is worse than none.
    #[test]
    fn a_popup_taller_than_the_frame_scrolls() {
        let lines: Vec<Line> = (0..40)
            .map(|i| Line::from(Span::raw(format!("line {i}"))))
            .collect();

        let top = scroll_to(lines.clone(), 0, 10);
        assert_eq!(top.len(), 10);
        assert_eq!(top[0].to_string(), "line 0");

        let bottom = scroll_to(lines.clone(), 39, 10);
        assert_eq!(bottom.len(), 10);
        assert_eq!(
            bottom.last().unwrap().to_string(),
            "line 39",
            "the end is reachable"
        );

        assert_eq!(scroll_to(lines, 0, 0).len(), 40, "no height, no window");
    }

    /// The window has to follow the very first cursor step, with no dead zone — a popup
    /// with no highlighted cursor (Log, Help, Details) would otherwise look stuck for a
    /// press or two before the content visibly moves.
    #[test]
    fn the_window_follows_the_cursor_from_the_first_step() {
        let lines: Vec<Line> = (0..40)
            .map(|i| Line::from(Span::raw(format!("line {i}"))))
            .collect();

        let after_one_step = scroll_to(lines, 1, 10);
        assert_eq!(
            after_one_step[0].to_string(),
            "line 1",
            "no dead zone: the very first cursor step already scrolls the window"
        );
    }

    /// The mirror of the test above: scrolling *up* from the last line must respond just
    /// as fast as scrolling down from the first one does. Truncating `cursor` at
    /// `last_start` used to dump the whole compression on this end, so `k` right after
    /// `shift-G` sat dead for several presses before the window actually moved.
    #[test]
    fn the_window_follows_the_cursor_up_from_the_last_line_too() {
        let lines: Vec<Line> = (0..40)
            .map(|i| Line::from(Span::raw(format!("line {i}"))))
            .collect();

        let at_the_end = scroll_to(lines.clone(), 39, 10);
        assert_eq!(at_the_end.last().unwrap().to_string(), "line 39");

        let after_one_step_up = scroll_to(lines, 38, 10);
        assert_ne!(
            after_one_step_up.last().unwrap().to_string(),
            "line 39",
            "no dead zone: the very first step up from the end already scrolls the window"
        );
    }

    /// The help popup's cursor is bounded by the lines it actually has. Left unbounded,
    /// `j` runs past the end and needs as many `k` presses to come back.
    #[test]
    fn the_help_cursor_stops_at_the_last_line() {
        let mut state = state();
        open(&mut state, Action::Help);

        let last = action::help_lines(&keymap()).len() - 1;
        for _ in 0..500 {
            press(&mut state, KeyCode::Char('j'));
        }
        assert_eq!(state.mode.popup_state().unwrap().cursor, last);

        press(&mut state, KeyCode::Char('k'));
        assert_eq!(
            state.mode.popup_state().unwrap().cursor,
            last - 1,
            "one press comes back one line"
        );
    }

    /// The log opens on its newest line, and `k` scrolls up from there rather than
    /// jumping to the top.
    #[test]
    fn the_log_opens_at_the_end_and_scrolls_up_from_it() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();

        let mut state = state();
        state.log.seed(&refs);
        open(&mut state, Action::LogMenu);

        assert_eq!(state.mode.popup_state().unwrap().cursor, 19);
        press(&mut state, KeyCode::Char('k'));
        assert_eq!(
            state.mode.popup_state().unwrap().cursor,
            18,
            "k scrolls up from the end, not to the top"
        );

        let shown = text(&state);
        assert!(
            shown.contains("line 19"),
            "the newest line is visible: {shown}"
        );
    }

    /// ctrl-d / ctrl-u page a popup exactly like the named PageDown/PageUp keys.
    #[test]
    fn ctrl_d_and_ctrl_u_page_the_log_popup() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();

        let mut state = state();
        state.log.seed(&refs);
        open(&mut state, Action::LogMenu);
        assert_eq!(state.mode.popup_state().unwrap().cursor, 19);

        press_ctrl(&mut state, KeyCode::Char('u'));
        assert_eq!(state.mode.popup_state().unwrap().cursor, 9);

        press_ctrl(&mut state, KeyCode::Char('d'));
        assert_eq!(state.mode.popup_state().unwrap().cursor, 19);
    }

    /// `g` / `shift-G` jump to the top and bottom of a read-only text popup.
    #[test]
    fn g_and_shift_g_jump_to_the_ends_of_the_log_popup() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();

        let mut state = state();
        state.log.seed(&refs);
        open(&mut state, Action::LogMenu);

        press(&mut state, KeyCode::Char('g'));
        assert_eq!(state.mode.popup_state().unwrap().cursor, 0);

        press(&mut state, KeyCode::Char('G'));
        assert_eq!(state.mode.popup_state().unwrap().cursor, 19);
    }

    /// `g` still jumps to a matching skin name rather than to the top of the list — the
    /// letter shortcut and the read-only popups' Top binding must not collide.
    #[test]
    fn g_still_jumps_to_a_skin_by_letter() {
        let mut state = state();
        open(&mut state, Action::SkinMenu);

        press(&mut state, KeyCode::Char('g'));

        let cursor = state.mode.popup_state().unwrap().cursor;
        assert_eq!(crate::ui::skins::BUILTIN_NAMES[cursor], "gruvbox-dark");
    }

    /// Same for Sort: `d` must still jump to the `DIFF` column, not page down.
    #[test]
    fn d_still_jumps_to_a_sort_column_by_letter() {
        let mut state = state();
        open(&mut state, Action::SortMenu);

        press(&mut state, KeyCode::Char('d'));

        let cursor = state.mode.popup_state().unwrap().cursor;
        assert_eq!(SORTABLE[cursor], Column::Diff);
    }

    /// One structure feeds both the renderer and the scroll bound, so they cannot drift.
    #[test]
    fn the_rendered_help_has_a_line_per_generated_entry() {
        let entries = action::help_lines(&keymap());
        let lines = help_lines(&keymap(), &theme(), 80);
        assert_eq!(entries.len(), lines.len());
    }

    /// A dragged window edge passes through every size on the way.
    #[test]
    fn degenerate_sizes_do_not_panic() {
        let mut state = state();
        open(&mut state, Action::Help);

        for width in 1..40u16 {
            for height in 1..12u16 {
                let lines = rendered(&state, width, height);
                assert_eq!(lines.len(), usize::from(height));
            }
        }
    }
}
