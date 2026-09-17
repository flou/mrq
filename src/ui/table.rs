//! The merge-request table.
//!
//! Cell content, row styling, and the truncation that has to
//! measure display width rather than characters.
//!
//! # Width is not length
//!
//! Merge request titles contain CJK, emoji and combining marks. `str::len` counts bytes,
//! `chars().count()` counts code points, and neither is the number of terminal cells a
//! string occupies — a CJK character takes two, a combining accent takes none. Getting
//! this wrong does not truncate slightly early, it shifts every column to the right of
//! the title on that row, which corrupts the whole table.

use std::borrow::Cow;
use std::num::NonZeroU16;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::state::Tab;
use crate::config::schema::Column;
use crate::gitlab::model::MergeRequest;
use crate::term::hyperlink;
use crate::ui::columns::{self, Allocation};
use crate::ui::theme::{Role, Theme};

/// Cells the gutter takes out of the table area before any column gets one: just the
/// marker itself — it sits flush against the first column, with no gap of its own.
///
/// Outside the [`columns`] budget, and it has to be: a table of `k` columns occupies
/// `1 + sum(widths) + sum(gaps)` cells, while an [`Allocation`] describes `sum(widths) +
/// sum(gaps)` — the gaps sized per boundary by [`columns::gap_between`], not uniformly.
/// Allocating against the full area asks ratatui's layout for one cell it does not have,
/// and the solver takes it back from whichever column it likes — invisibly, since the
/// rendered line is still exactly as wide as the area.
pub const GUTTER_WIDTH: u16 = 1;

/// Allocate column widths for a table drawn into `width` cells.
///
/// The only correct way to build an [`Allocation`] for [`build`]; calling
/// [`columns::allocate`] with the raw area width silently overcommits by [`GUTTER_WIDTH`],
/// and skips the `author`/`repo` measurements below.
///
/// `rows` should be the tab's full row set, not the visible window — measuring only what
/// is on screen would make a column resize while scrolling.
pub fn allocate(columns: &[Column], width: u16, rows: &[&MergeRequest]) -> Allocation {
    let fitted = columns::Fitted {
        author: author_fit(rows),
        repo: repo_fit(rows),
    };
    columns::allocate(columns, width.saturating_sub(GUTTER_WIDTH), fitted)
}

/// The `author` column's content width: the widest author name on `rows`, floored at the
/// header so `AUTHOR` is never truncated and capped at [`columns::FIT_MAX`].
fn author_fit(rows: &[&MergeRequest]) -> Option<u16> {
    content_fit(
        rows.iter().map(|mr| mr.author.username.width()),
        Column::Author,
    )
}

/// The `repo` column's content width: the widest project name on `rows`, floored at the
/// header so `REPO` is never truncated and capped at [`columns::FIT_MAX`].
fn repo_fit(rows: &[&MergeRequest]) -> Option<u16> {
    content_fit(rows.iter().map(|mr| mr.project_name.width()), Column::Repo)
}

/// Shared by [`author_fit`] and [`repo_fit`]: the widest of `widths`, floored at
/// `column`'s header (so the header is never truncated) and capped at
/// [`columns::FIT_MAX`].
///
/// `None` for an empty table — there is nothing to measure, and collapsing to the header
/// width would make the column jump the moment the first row arrives.
///
/// Takes an iterator of already-measured display widths rather than strings, so the
/// caller picks the field and this stays a plain fold — measuring in display cells, not
/// bytes or code points, is the caller's job, for the reason the module doc gives: a
/// username or repo name can contain CJK or combining marks just as a title can.
fn content_fit(widths: impl Iterator<Item = usize>, column: Column) -> Option<u16> {
    let widest = widths.max()?;
    let header = column.header().width();
    let clamped = widest.max(header).min(columns::FIT_MAX as usize);
    u16::try_from(clamped).ok()
}

/// Compact relative time: `45s`, `12m`, `6h`, `3d`.
///
/// Weeks and beyond collapse to days rather than gaining their own unit. A merge request
/// open for 400 days and one open for 500 are equally stale; the distinction costs a
/// column of width and tells the reader nothing.
pub fn relative_time(from: jiff::Timestamp, now: jiff::Timestamp) -> String {
    compact_seconds(now.as_second() - from.as_second())
}

/// A span of seconds in the same compact form, for durations that are not timestamps —
/// the status bar's "next in" and "retrying in".
pub fn compact_seconds(seconds: i64) -> String {
    if seconds < 0 {
        // Clock skew between the instance and this machine, or a deadline that has just
        // passed. "now" is less wrong than a negative age.
        return "now".to_owned();
    }

    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Truncate to a display width, appending an ellipsis when anything was cut.
pub fn truncate(text: &str, width: usize, ellipsis: &str) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let ellipsis_width = ellipsis.width();
    if width <= ellipsis_width {
        // No room for content plus a marker; take whatever fits, marker included.
        return take_width(text, width);
    }

    let mut out = take_width(text, width - ellipsis_width);
    out.push_str(ellipsis);
    out
}

/// The longest prefix of `text` that fits in `width` terminal cells.
fn take_width(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        // `ch.width()` rather than `ch.to_string().width()`: this loop already measures
        // one char at a time, so the two agree, and the former does not heap-allocate a
        // `String` per character of every truncated cell on every draw.
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Pad to a display width, so columns line up regardless of what the cell contains.
fn pad(text: &str, width: usize) -> String {
    let mut out = truncate(text, width, "");
    let deficit = width.saturating_sub(out.width());
    out.push_str(&" ".repeat(deficit));
    out
}

/// The APRV cell.
///
/// Takes the theme for its mark: on a terminal running the ascii theme, a `✓` is not a
/// tick, it is a replacement character in a fixed-width column.
fn approved_cell(mr: &MergeRequest, theme: &Theme) -> String {
    if mr.approved {
        theme.approved_mark().to_owned()
    } else {
        String::new()
    }
}

/// The DIFF cell.
fn diff_cell(mr: &MergeRequest) -> String {
    format!("+{} -{}", mr.additions, mr.deletions)
}

/// The first letter of `first`, uppercased, as a `String` — the leading half of a
/// trigram built from two words.
fn initial(word: &str) -> String {
    word.chars().take(1).collect::<String>().to_uppercase()
}

/// The first `count` characters of `text`, uppercased. The fallback for a name (or
/// username) that split gave only one word to work with.
fn uppercase_prefix(text: &str, count: usize) -> String {
    text.chars().take(count).collect::<String>().to_uppercase()
}

/// The assignee's trigram: first letter of the first word plus the first two of the
/// last, uppercased (`Charles Billow` -> `CBI`). A single-word name, or a username with
/// no display name, contributes its own first three characters instead
/// (`charles.billow` -> `CBI`).
///
/// `name` is split on whitespace; `username` — used only when there is no name — is
/// split on the separators GitLab usernames use (`.`, `_`, `-`), since it never contains
/// spaces.
fn trigram(name: Option<&str>, username: &str) -> String {
    let name_words: Vec<&str> = name.into_iter().flat_map(str::split_whitespace).collect();

    if !name_words.is_empty() {
        return match name_words.as_slice() {
            [single] => uppercase_prefix(single, 3),
            [first, .., last] => initial(first) + &uppercase_prefix(last, 2),
            [] => unreachable!("checked non-empty above"),
        };
    }

    let username_words: Vec<&str> = username
        .split(['.', '_', '-'])
        .filter(|w| !w.is_empty())
        .collect();

    match username_words.as_slice() {
        [] => uppercase_prefix(username, 3),
        [single] => uppercase_prefix(single, 3),
        [first, .., last] => initial(first) + &uppercase_prefix(last, 2),
    }
}

/// The ASSIGNED column has no one to name.
const UNASSIGNED_MARK: &str = "-";

/// The ASSIGNED cell: `Yes`/`No` by default, or the first assignee's trigram when
/// `[ui].assignee_trigram` is on.
fn assigned_cell(mr: &MergeRequest, trigram_mode: bool) -> Cow<'_, str> {
    if !trigram_mode {
        return Cow::Borrowed(if mr.assigned_to_me() { "Yes" } else { "No" });
    }
    match mr.first_assignee() {
        Some(user) => Cow::Owned(trigram(user.name.as_deref(), &user.username)),
        None => Cow::Borrowed(UNASSIGNED_MARK),
    }
}

/// The text for one cell, before padding or styling.
///
/// Borrows straight from `mr` for the columns that are a plain field, rather than
/// cloning a `String` that `truncate`/`pad` immediately replace with a freshly built one
/// anyway — every other column already has to allocate, so only the passthrough columns
/// gain anything, but the table draws every visible row of every column on every frame.
fn cell_text<'a>(
    mr: &'a MergeRequest,
    column: Column,
    theme: &Theme,
    now: jiff::Timestamp,
    trigram_mode: bool,
) -> Cow<'a, str> {
    match column {
        Column::Approved => Cow::Owned(approved_cell(mr, theme)),
        Column::Author => Cow::Borrowed(mr.author.username.as_str()),
        Column::Repo => Cow::Borrowed(mr.project_name.as_str()),
        Column::Title => {
            if mr.draft {
                Cow::Owned(format!("[Draft] {}", mr.title))
            } else {
                Cow::Borrowed(mr.title.as_str())
            }
        }
        // Filled in by the caller, which has the theme and therefore the glyph set.
        Column::Pipeline => Cow::Borrowed(""),
        Column::Assigned => assigned_cell(mr, trigram_mode),
        Column::Age => Cow::Owned(relative_time(mr.created_at, now)),
        Column::Updated => Cow::Owned(relative_time(mr.updated_at, now)),
        Column::Diff => Cow::Owned(diff_cell(mr)),
        Column::Branch => Cow::Borrowed(mr.source_branch.as_str()),
    }
}

/// Which role a cell's text takes.
const fn cell_role(mr: &MergeRequest, column: Column) -> Role {
    if mr.draft {
        // Drafts are dimmed wholesale: they are present for completeness, not for
        // action, and colouring their pipeline green invites reading them as ready.
        return Role::Dim;
    }
    match column {
        // A conflicted or unmergeable title is a problem, not a failure.
        Column::Title if mr.is_blocked() => Role::Warning,
        Column::Diff => Role::Normal,
        _ => Role::Normal,
    }
}

/// What the gutter shows for a row.
const fn gutter(theme: &Theme, selected: bool, is_new: bool) -> &str {
    if selected {
        theme.selection_marker()
    } else if is_new {
        theme.new_marker()
    } else {
        theme.blank_marker()
    }
}

/// Build the header row.
fn header<'a>(allocation: &Allocation, theme: &Theme) -> Row<'a> {
    let style = theme.style(Role::ColumnHeader);
    let mut cells = vec![Cell::from(" ")];

    for (index, (column, width)) in allocation.widths.iter().enumerate() {
        // No gap before the first column: it sits flush against the gutter. Ratatui's
        // own `column_spacing` is uniform, so gaps from here on are drawn explicitly.
        if index > 0 {
            let previous = allocation.widths[index - 1].0;
            let gap = columns::gap_between(previous, *column);
            cells.push(Cell::from(" ".repeat(gap as usize)).style(style));
        }
        cells.push(Cell::from(pad(column.header(), *width as usize)).style(style));
    }
    Row::new(cells).style(style)
}

/// Build one data row.
fn row<'a>(
    mr: &MergeRequest,
    allocation: &Allocation,
    theme: &Theme,
    selected: bool,
    is_new: bool,
    now: jiff::Timestamp,
    trigram_mode: bool,
) -> Row<'a> {
    let gutter_cell = Cell::from(gutter(theme, selected, is_new).to_owned());
    let gutter_cell = if selected {
        gutter_cell.style(theme.style(Role::Marker))
    } else {
        gutter_cell
    };
    let mut cells = vec![gutter_cell];

    for (index, (column, width)) in allocation.widths.iter().enumerate() {
        if index > 0 {
            let previous = allocation.widths[index - 1].0;
            let gap = columns::gap_between(previous, *column);
            cells.push(Cell::from(" ".repeat(gap as usize)));
        }
        let width = *width as usize;
        let cell = match column {
            // The pipeline cell is a glyph with its own role, independent of the row's.
            Column::Pipeline => {
                let (glyph, role) = theme.pipeline(mr.pipeline.as_ref().map(|p| &p.status));
                let role = if mr.draft { Role::Dim } else { role };
                Cell::from(pad(glyph, width)).style(theme.style(role))
            }
            // Additions and deletions are coloured separately, so the cell is two spans.
            Column::Diff if !mr.draft => diff_spans(mr, width, theme),
            // Bold on top of `Role::Success`'s green, so an approved MR stands out at a
            // glance rather than blending into the rest of the row.
            Column::Approved if mr.approved && !mr.draft => {
                let text = truncate(
                    &cell_text(mr, *column, theme, now, trigram_mode),
                    width,
                    theme.ellipsis(),
                );
                Cell::from(pad(&text, width)).style(theme.emphasise(Role::Success))
            }
            // Green, not bold: bold is APRV's signal, and this only marks the row as
            // yours, the same fact `Yes`/`No` carried before the trigram existed.
            Column::Assigned if trigram_mode && mr.assigned_to_me() && !mr.draft => {
                let text = truncate(
                    &cell_text(mr, *column, theme, now, trigram_mode),
                    width,
                    theme.ellipsis(),
                );
                Cell::from(pad(&text, width)).style(theme.style(Role::Success))
            }
            other => {
                let text = truncate(
                    &cell_text(mr, *other, theme, now, trigram_mode),
                    width,
                    theme.ellipsis(),
                );
                Cell::from(pad(&text, width)).style(theme.style(cell_role(mr, *other)))
            }
        };
        cells.push(cell);
    }

    let mut row = Row::new(cells);
    if selected {
        row = row.style(theme.style(Role::Selection));
    } else if mr.draft {
        row = row.style(theme.style(Role::Dim));
    }
    row
}

fn diff_spans<'a>(mr: &MergeRequest, width: usize, theme: &Theme) -> Cell<'a> {
    let added = format!("+{}", mr.additions);
    let removed = format!("-{}", mr.deletions);

    // Drop the colouring rather than the numbers when the column is tight: a truncated
    // "+12 -" is worse than an uncoloured "+12 -3".
    let combined = format!("{added} {removed}");
    if combined.width() > width {
        return Cell::from(pad(&truncate(&combined, width, ""), width));
    }

    let padding = " ".repeat(width - combined.width());
    Cell::from(Line::from(vec![
        Span::styled(added, theme.style(Role::Added)),
        Span::raw(" "),
        Span::styled(removed, theme.style(Role::Removed)),
        Span::raw(padding),
    ]))
}

/// Build the table widget for a set of rows.
///
/// `rows` is already filtered and sorted; this module only draws.
pub fn build<'a>(
    rows: &[&MergeRequest],
    tab: &Tab,
    allocation: &Allocation,
    theme: &Theme,
    now: jiff::Timestamp,
    trigram_mode: bool,
) -> Table<'a> {
    let selected = tab.selected_id();

    let body: Vec<Row> = rows
        .iter()
        .copied()
        .map(|mr| {
            row(
                mr,
                allocation,
                theme,
                selected == Some(mr.id.as_str()),
                tab.is_new(&mr.id),
                now,
                trigram_mode,
            )
        })
        .collect();

    // One leading constraint for the gutter, then the allocated widths with an explicit
    // gap constraint between each pair — sized per boundary by `columns::gap_between`,
    // not uniformly — but not before the first column, which sits flush against the
    // gutter. Lengths, not percentages: the allocation has already decided, and letting
    // ratatui redistribute would undo it.
    let mut constraints = vec![Constraint::Length(1)];
    for (index, (column, width)) in allocation.widths.iter().enumerate() {
        if index > 0 {
            let previous = allocation.widths[index - 1].0;
            constraints.push(Constraint::Length(columns::gap_between(previous, *column)));
        }
        constraints.push(Constraint::Length(*width));
    }

    Table::new(body, constraints)
        .header(header(allocation, theme))
        // Gaps are drawn as explicit constraints above, so ratatui adds none of its own.
        // No row striping: it fights with terminal themes.
        .column_spacing(0)
}

/// How many body rows fit, given the header.
pub const fn visible_row_count(area: Rect) -> usize {
    area.height.saturating_sub(1) as usize
}

/// The slice of rows the table should draw for one frame.
///
/// [`build`] and [`link_titles`] both render `rows[skip..]`, so they have to agree on
/// the window or a title would link to the wrong merge request. The scroll offset is
/// [`Tab::scroll`](crate::app::state::Tab::scroll), kept on the selection by `App`.
pub fn windowed<T>(rows: &[T], scroll: usize, viewport: usize) -> &[T] {
    let start = scroll.min(rows.len());
    let end = start.saturating_add(viewport).min(rows.len());
    &rows[start..end]
}

/// The exact text [`row`] draws in a title cell, before padding.
///
/// [`link_titles`] needs this again: the clickable region has to stop where the visible
/// title does, not run on into the padding that fills out the rest of the column.
fn title_text(mr: &MergeRequest, width: usize, theme: &Theme, now: jiff::Timestamp) -> String {
    // The trigram flag only affects `Column::Assigned`, so its value here is moot.
    truncate(
        &cell_text(mr, Column::Title, theme, now, false),
        width,
        theme.ellipsis(),
    )
}

/// Make each row's title a clickable link to its merge request.
///
/// Applied to the rendered buffer rather than built into the cells, because ratatui
/// splits a widget's text into graphemes and gives each one a cell — an escape sequence
/// placed in the text would be cut across cells and printed as garbage.
///
/// Call only when the terminal advertises OSC 8. An unsupported terminal does not ignore
/// the escape, it prints it. Also skip it while a popup is on screen — see the caller.
pub fn link_titles(
    buffer: &mut Buffer,
    area: Rect,
    rows: &[&MergeRequest],
    allocation: &Allocation,
    theme: &Theme,
    now: jiff::Timestamp,
) {
    let (Some(x), Some(width)) = (
        column_x(allocation, area, Column::Title),
        allocation.width_of(Column::Title),
    ) else {
        return;
    };

    for (index, mr) in rows.iter().copied().enumerate() {
        // The header takes the first row of the area.
        let Ok(offset) = u16::try_from(index + 1) else {
            break;
        };
        if offset >= area.height {
            break;
        }
        let y = area.y + offset;

        let text = title_text(mr, width as usize, theme, now);
        let Ok(text_width) = u16::try_from(text.width()) else {
            continue;
        };
        if text_width == 0 {
            continue;
        }

        // Computed from `text` itself, not found by scanning the buffer: ratatui fills a
        // wide grapheme's trailing cell with a literal space, not an empty symbol, so
        // "the last non-blank cell" is indistinguishable from ordinary padding — and that
        // trailing cell is skipped by the terminal diff regardless of what is written
        // into it, because the diff advances past it on the *preceding* cell's width, not
        // this one's content. Ending on it would silently drop the close.
        let last_width = text
            .chars()
            .next_back()
            .and_then(|c| c.width())
            .unwrap_or(1);
        let close_x = if last_width >= 2 {
            x + text_width - 2
        } else {
            x + text_width - 1
        };

        if close_x == x {
            // The whole title is a single cell (a lone wide glyph, or one narrow
            // character): open and close land on the same cell. Combine them into one
            // rewrite — a second call would measure the width of the string the first
            // call already rewrote, not the original glyph's.
            rewrite(buffer, x, y, |symbol| {
                format!(
                    "{}{symbol}{}",
                    hyperlink::open(&mr.id, &mr.web_url),
                    hyperlink::CLOSE
                )
            });
        } else {
            rewrite(buffer, x, y, |symbol| {
                format!("{}{symbol}", hyperlink::open(&mr.id, &mr.web_url))
            });
            rewrite(buffer, close_x, y, |symbol| {
                format!("{symbol}{}", hyperlink::CLOSE)
            });
        }
    }
}

/// Rewrite one cell's symbol, pinning the width it had before.
///
/// The escapes are zero-width on the terminal but not to `unicode_width`, which is what
/// ratatui's diff uses to decide how many cells a symbol covers. Without the pin it skips
/// everything to the right of the link on the next redraw.
fn rewrite(buffer: &mut Buffer, x: u16, y: u16, with: impl FnOnce(&str) -> String) {
    let Some(cell) = buffer.cell_mut((x, y)) else {
        return;
    };
    let width = NonZeroU16::new(cell.symbol().width() as u16).unwrap_or(NonZeroU16::MIN);
    let symbol = with(cell.symbol());

    cell.set_symbol(&symbol)
        .set_diff_option(CellDiffOption::ForcedWidth(width));
}

/// Where a column's first cell lands inside the table area.
///
/// Mirrors the constraints [`build`] hands to ratatui: the gutter, then each column with
/// its `columns::gap_between` spacing before it — except the first, which sits flush
/// against the gutter.
pub fn column_x(allocation: &Allocation, area: Rect, column: Column) -> Option<u16> {
    let mut x = area.x + GUTTER_WIDTH;
    for (index, (candidate, width)) in allocation.widths.iter().enumerate() {
        if index > 0 {
            let previous = allocation.widths[index - 1].0;
            x += columns::gap_between(previous, *candidate);
        }
        if *candidate == column {
            return Some(x);
        }
        x += *width;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Filter, Scope, Sort};
    use crate::gitlab::model::{Pipeline, PipelineStatus, User, fixtures::mr};
    use crate::term::caps::{Capabilities, ColorDepth, NotifyEscape};
    use ratatui::backend::{CrosstermBackend, TestBackend};
    use ratatui::{Terminal, TerminalOptions, Viewport};

    fn now() -> jiff::Timestamp {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn theme(ascii: bool) -> Theme {
        Theme::builtin(
            "catppuccin-mocha",
            ascii,
            &Capabilities {
                color: ColorDepth::TrueColor,
                hyperlinks: false,
                notify: NotifyEscape::None,
                focus_events: true,
                multiplexed: false,
                over_ssh: false,
            },
        )
    }

    fn tab() -> Tab {
        Tab::new(
            0,
            &Filter::named("Assigned", Scope::Assigned),
            Sort::default(),
            false,
        )
    }

    /// Render to a fixed-size backend and return the visible text, line by line.
    fn render(rows: &[MergeRequest], tab: &Tab, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = theme(false);
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, width, &refs);

        terminal
            .draw(|frame| {
                let table = build(&refs, tab, &allocation, &theme, now(), false);
                frame.render_widget(table, frame.area());
            })
            .unwrap();

        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|line| line.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn relative_times_use_the_documented_units() {
        let base: jiff::Timestamp = "2026-09-11T12:00:00Z".parse().unwrap();
        let ago = |secs: i64| {
            relative_time(
                jiff::Timestamp::from_second(base.as_second() - secs).unwrap(),
                base,
            )
        };

        assert_eq!(ago(0), "0s");
        assert_eq!(ago(45), "45s");
        assert_eq!(ago(59), "59s");
        assert_eq!(ago(60), "1m");
        assert_eq!(ago(12 * 60), "12m");
        assert_eq!(ago(3_600), "1h");
        assert_eq!(ago(6 * 3_600), "6h");
        assert_eq!(ago(86_400), "1d");
        assert_eq!(ago(3 * 86_400), "3d");
        assert_eq!(ago(400 * 86_400), "400d", "weeks collapse into days");
    }

    /// Instance and client clocks disagree; a negative age is worse than "now".
    #[test]
    fn a_future_timestamp_reads_as_now() {
        let base: jiff::Timestamp = "2026-09-11T12:00:00Z".parse().unwrap();
        let future = jiff::Timestamp::from_second(base.as_second() + 600).unwrap();

        assert_eq!(relative_time(future, base), "now");
    }

    /// The bug this guards against does not truncate slightly early — it shifts every
    /// column to the right of the title and corrupts the row.
    #[test]
    fn truncation_measures_display_width_not_characters() {
        // Each CJK character occupies two cells.
        let cjk = "データベース移行";
        assert_eq!(cjk.chars().count(), 8);
        assert_eq!(cjk.width(), 16);

        let truncated = truncate(cjk, 10, "…");
        assert!(
            truncated.width() <= 10,
            "`{truncated}` is {} cells wide",
            truncated.width()
        );
    }

    #[test]
    fn truncation_leaves_short_text_alone() {
        assert_eq!(truncate("short", 20, "…"), "short");
        assert_eq!(truncate("exact", 5, "…"), "exact");
    }

    #[test]
    fn truncation_appends_an_ellipsis_only_when_it_cuts() {
        let cut = truncate("a very long merge request title", 10, "…");
        assert!(cut.ends_with('…'), "{cut}");
        assert!(cut.width() <= 10);
    }

    /// A width with no room for content plus a marker must still not overflow.
    #[test]
    fn truncation_survives_absurdly_narrow_widths() {
        for width in 0..4 {
            let out = truncate("something long", width, "…");
            assert!(out.width() <= width, "width {width} produced `{out}`");
        }
    }

    #[test]
    fn padding_fills_to_the_exact_display_width() {
        assert_eq!(pad("ab", 5).width(), 5);
        assert_eq!(pad("データ", 8).width(), 8, "wide characters counted");
        assert_eq!(pad("too long for this", 5).width(), 5);
    }

    /// No rows, nothing to measure — falls back to the fixed width in `columns::rules`
    /// rather than collapsing to the header, which would make the column jump the moment
    /// the first row arrives.
    #[test]
    fn author_fit_has_no_opinion_on_an_empty_table() {
        assert_eq!(author_fit(&[]), None);
    }

    /// Shorter than the header never shrinks the column below `AUTHOR` itself.
    #[test]
    fn author_fit_is_floored_at_the_header_width() {
        let short = mr("a", "bo");
        assert_eq!(
            author_fit(&[&short]),
            Some(Column::Author.header().width() as u16)
        );
    }

    /// Longer than `FIT_MAX` is capped, so one outlier username cannot eat the title.
    #[test]
    fn author_fit_is_capped_at_the_maximum() {
        let long = mr("a", &"x".repeat(40));
        assert_eq!(author_fit(&[&long]), Some(columns::FIT_MAX));
    }

    /// The widest name wins, not the first or the last.
    #[test]
    fn author_fit_measures_the_widest_name_on_the_rows() {
        let short = mr("a", "bo");
        let long = mr("b", "a-longer-username");
        assert_eq!(
            author_fit(&[&short, &long]),
            Some("a-longer-username".width() as u16)
        );
    }

    /// A CJK username occupies two cells per character, not one — the same rule as titles.
    #[test]
    fn author_fit_measures_display_width_not_characters() {
        let m = mr("a", "データ");
        assert_eq!(author_fit(&[&m]), Some("データ".width() as u16));
    }

    /// Between the header floor and `FIT_MAX`, TITLE gets back whatever AUTHOR does not
    /// need — the point of fitting the column to its content in the first place.
    #[test]
    fn a_short_author_leaves_more_room_for_the_title() {
        let short = mr("a", "bo");
        let long = mr("b", &"x".repeat(25));

        let narrow_author = allocate(&Column::DEFAULT, 120, &[&short]);
        let wide_author = allocate(&Column::DEFAULT, 120, &[&long]);

        let narrow_title = narrow_author.width_of(Column::Title).unwrap();
        let wide_title = wide_author.width_of(Column::Title).unwrap();

        assert!(
            narrow_title > wide_title,
            "a short author ({narrow_title}) should leave more room for the title than a long one ({wide_title})"
        );
    }

    /// `repo_fit` mirrors `author_fit`: no rows, nothing to measure.
    #[test]
    fn repo_fit_has_no_opinion_on_an_empty_table() {
        assert_eq!(repo_fit(&[]), None);
    }

    /// Shorter than the header never shrinks the column below `REPO` itself.
    #[test]
    fn repo_fit_is_floored_at_the_header_width() {
        let mut short = mr("a", "someone");
        short.project_name = "x".into();
        assert_eq!(
            repo_fit(&[&short]),
            Some(Column::Repo.header().width() as u16)
        );
    }

    /// Longer than `FIT_MAX` is capped, so one outlier project name cannot eat the title.
    #[test]
    fn repo_fit_is_capped_at_the_maximum() {
        let mut long = mr("a", "someone");
        long.project_name = "x".repeat(40);
        assert_eq!(repo_fit(&[&long]), Some(columns::FIT_MAX));
    }

    /// The widest name wins, not the first or the last.
    #[test]
    fn repo_fit_measures_the_widest_name_on_the_rows() {
        let mut short = mr("a", "someone");
        short.project_name = "x".into();
        let mut long = mr("b", "someone");
        long.project_name = "a-longer-project-name".into();

        assert_eq!(
            repo_fit(&[&short, &long]),
            Some("a-longer-project-name".width() as u16)
        );
    }

    /// A CJK project name occupies two cells per character, not one — the same rule as
    /// titles and authors.
    #[test]
    fn repo_fit_measures_display_width_not_characters() {
        let mut m = mr("a", "someone");
        m.project_name = "データ".into();
        assert_eq!(repo_fit(&[&m]), Some("データ".width() as u16));
    }

    /// Between the header floor and `FIT_MAX`, TITLE gets back whatever REPO does not
    /// need, exactly as it does for AUTHOR.
    #[test]
    fn a_short_repo_leaves_more_room_for_the_title() {
        let mut short = mr("a", "someone");
        short.project_name = "x".into();
        let mut long = mr("b", "someone");
        long.project_name = "x".repeat(25);

        let narrow_repo = allocate(&Column::DEFAULT, 120, &[&short]);
        let wide_repo = allocate(&Column::DEFAULT, 120, &[&long]);

        let narrow_title = narrow_repo.width_of(Column::Title).unwrap();
        let wide_title = wide_repo.width_of(Column::Title).unwrap();

        assert!(
            narrow_title > wide_title,
            "a short repo ({narrow_title}) should leave more room for the title than a long one ({wide_title})"
        );
    }

    /// Two modes.
    #[test]
    fn the_approved_column_has_two_modes() {
        let mut approved = mr("a", "someone");
        approved.approved = true;
        assert_eq!(approved_cell(&approved, &theme(false)), "✔");

        let mut pending = mr("b", "someone");
        pending.approved = false;
        assert_eq!(
            approved_cell(&pending, &theme(false)),
            "",
            "not approved is blank, not a dash"
        );
    }

    /// The ascii theme is chosen by terminals that cannot draw anything else, so every
    /// cell it produces has to be ascii — including the ones whose glyph reads as
    /// punctuation rather than as a symbol.
    #[test]
    fn no_cell_is_non_ascii_under_the_ascii_theme() {
        let ascii = theme(true);
        let mut m = mr("gid://1", "jdoe");
        m.approved = true;
        m.approved_by = vec!["jdoe".to_owned()];

        for column in Column::DEFAULT {
            let text = cell_text(&m, column, &ascii, now(), false);
            assert!(text.is_ascii(), "{column:?} rendered `{text}`");
        }

        let mut not_approved = m;
        not_approved.approved = false;
        assert!(approved_cell(&not_approved, &ascii).is_ascii());
    }

    #[test]
    fn cells_carry_the_documented_content() {
        let mut m = mr("a", "jdoe");
        m.project_name = "web-app".into();
        m.title = "Add dark mode".into();
        m.additions = 310;
        m.deletions = 4;
        m.assignees = vec![User::new("me")];
        m.recompute_derived("me");

        assert_eq!(
            cell_text(&m, Column::Author, &theme(false), now(), false),
            "jdoe"
        );
        assert_eq!(
            cell_text(&m, Column::Repo, &theme(false), now(), false),
            "web-app"
        );
        assert_eq!(
            cell_text(&m, Column::Title, &theme(false), now(), false),
            "Add dark mode"
        );
        assert_eq!(
            cell_text(&m, Column::Assigned, &theme(false), now(), false),
            "Yes"
        );
        assert_eq!(
            cell_text(&m, Column::Diff, &theme(false), now(), false),
            "+310 -4"
        );

        m.assignees.clear();
        m.recompute_derived("me");
        assert_eq!(
            cell_text(&m, Column::Assigned, &theme(false), now(), false),
            "No"
        );
    }

    #[test]
    fn drafts_are_prefixed_in_the_title() {
        let mut m = mr("a", "someone");
        m.draft = true;
        m.title = "Migration guide".into();

        assert_eq!(
            cell_text(&m, Column::Title, &theme(false), now(), false),
            "[Draft] Migration guide"
        );
        assert_eq!(cell_role(&m, Column::Title), Role::Dim);
    }

    /// The trigram, spelled out: `Charles Billow` -> `CBI`, `Leeroy Feist` -> `LFE` — the
    /// examples the option was requested with.
    #[test]
    fn trigram_combines_the_first_and_last_word() {
        assert_eq!(trigram(Some("Charles Billow"), "cbillow"), "CBI");
        assert_eq!(trigram(Some("Leeroy Feist"), "lfeist"), "LFE");
        assert_eq!(
            trigram(Some("Jean Claude Van Damme"), "jvandamme"),
            "JDA",
            "only the first and last word count, the middle ones are ignored"
        );
    }

    #[test]
    fn trigram_falls_back_to_the_username_without_a_name() {
        assert_eq!(trigram(None, "charles.billow"), "CBI");
        assert_eq!(trigram(Some(""), "leeroy_feist"), "LFE");
        assert_eq!(
            trigram(None, "asmith"),
            "ASM",
            "no separator at all: first three characters"
        );
    }

    #[test]
    fn trigram_handles_a_single_word_name() {
        assert_eq!(trigram(Some("Cher"), "cher"), "CHE");
    }

    #[test]
    fn trigram_does_not_panic_on_non_ascii() {
        // `.chars()`, not byte slicing: a non-ASCII first letter must not split a
        // multi-byte character.
        assert_eq!(trigram(Some("Éowyn Baggins"), "eowyn"), "ÉBA");
    }

    /// The ASG cell in trigram mode: the first assignee's trigram, `-` with none, and the
    /// legacy `Yes`/`No` text when the mode is off.
    #[test]
    fn the_assigned_column_has_two_modes() {
        let mut m = mr("a", "jdoe");
        m.assignees = vec![User {
            username: "cbillow".to_owned(),
            name: Some("Charles Billow".to_owned()),
        }];
        m.recompute_derived("nobody");

        assert_eq!(
            cell_text(&m, Column::Assigned, &theme(false), now(), true),
            "CBI"
        );
        assert_eq!(
            cell_text(&m, Column::Assigned, &theme(false), now(), false),
            "No",
            "the legacy text ignores who the assignee is"
        );

        m.assignees.clear();
        assert_eq!(
            cell_text(&m, Column::Assigned, &theme(false), now(), true),
            "-",
            "no assignee at all"
        );
    }

    /// A conflicted title is a problem, not a failure.
    #[test]
    fn a_blocked_title_takes_the_warning_role() {
        let mut m = mr("a", "someone");
        m.conflicts = true;

        assert_eq!(cell_role(&m, Column::Title), Role::Warning);
        assert_eq!(cell_role(&m, Column::Author), Role::Normal);
    }

    /// Colouring a draft's pipeline green invites reading it as ready.
    #[test]
    fn a_draft_dims_every_cell_including_the_pipeline() {
        let mut m = mr("a", "someone");
        m.draft = true;
        m.conflicts = true;

        assert_eq!(
            cell_role(&m, Column::Title),
            Role::Dim,
            "dim wins over warning"
        );
        assert_eq!(cell_role(&m, Column::Author), Role::Dim);
    }

    /// Reverse video plus a marker, so selection survives a terminal with
    /// weak reverse video.
    #[test]
    fn the_gutter_distinguishes_selected_new_and_ordinary_rows() {
        let theme = theme(false);

        assert_eq!(gutter(&theme, true, false), theme.selection_marker());
        assert_eq!(gutter(&theme, false, true), theme.new_marker());
        assert_eq!(gutter(&theme, false, false), theme.blank_marker());
        assert_eq!(
            gutter(&theme, true, true),
            theme.selection_marker(),
            "selection wins over newness"
        );
    }

    #[test]
    fn the_table_renders_a_header_and_rows() {
        let mut m = mr("a", "jdoe");
        m.title = "Fix retry backoff".into();
        m.project_name = "api-gateway".into();

        let lines = render(&[m], &tab(), 120, 5);

        assert!(lines[0].contains("AUTHOR"), "header: {:?}", lines[0]);
        assert!(lines[0].contains("TITLE"));
        assert!(lines[1].contains("jdoe"), "row: {:?}", lines[1]);
        assert!(lines[1].contains("Fix retry backoff"));
        assert!(lines[1].contains("api-gateway"));
    }

    #[test]
    fn the_selected_row_shows_its_marker() {
        let m = mr("gid://1", "jdoe");
        let mut tab = tab();
        tab.select(Some("gid://1".into()));

        let lines = render(&[m], &tab, 120, 5);
        let marker = theme(false).selection_marker();

        assert!(lines[1].starts_with(marker), "row: {:?}", lines[1]);
    }

    #[test]
    fn a_newly_arrived_row_is_marked_until_it_is_not_new() {
        let m = mr("gid://1", "jdoe");
        let mut tab = tab();
        tab.apply_rows(vec![mr("gid://0", "x")], std::time::Instant::now());
        tab.apply_rows(
            vec![mr("gid://0", "x"), m.clone()],
            std::time::Instant::now(),
        );

        let lines = render(std::slice::from_ref(&m), &tab, 120, 5);
        assert!(lines[1].starts_with('*'), "row: {:?}", lines[1]);

        // The marker lasts one refresh cycle: the next fetch brings nothing new, so it
        // goes (mrq-zv4.3).
        tab.apply_rows(
            vec![mr("gid://0", "x"), m.clone()],
            std::time::Instant::now(),
        );
        let lines = render(&[m], &tab, 120, 5);
        assert!(!lines[1].starts_with('*'), "row: {:?}", lines[1]);
    }

    #[test]
    fn the_pipeline_column_shows_its_glyph() {
        let mut m = mr("a", "jdoe");
        m.pipeline = Some(Pipeline {
            url: "https://example.com/p".into(),
            status: PipelineStatus::Failed,
            finished_at: None,
        });

        let lines = render(&[m], &tab(), 120, 5);
        assert!(lines[1].contains('✘'), "row: {:?}", lines[1]);
    }

    /// Rendering must not panic or overflow at any terminal width, including ones too
    /// narrow for most columns.
    #[test]
    fn the_table_renders_at_every_width() {
        let m = mr("a", "jdoe");

        for width in [20u16, 40, 60, 80, 100, 120, 200] {
            let lines = render(std::slice::from_ref(&m), &tab(), width, 4);
            for line in &lines {
                assert_eq!(
                    line.width(),
                    width as usize,
                    "width {width} produced a {}-cell line",
                    line.width()
                );
            }
        }
    }

    /// A title full of wide characters is the case that shifts columns if width is
    /// measured wrongly. Asserted by cell position, not by reconstructing the line:
    /// joining buffer symbols double-counts a wide character, because the terminal
    /// stores it in one cell while it occupies two.
    #[test]
    fn wide_characters_do_not_shift_the_columns() {
        let mut wide = mr("a", "jdoe");
        wide.title = "データベース移行のための変更".into();
        wide.additions = 11;
        wide.deletions = 22;

        let mut plain = mr("b", "jdoe");
        plain.title = "a plain ascii title".into();
        plain.additions = 33;
        plain.deletions = 44;

        let width = 120u16;
        let mut terminal = Terminal::new(TestBackend::new(width, 4)).unwrap();
        let theme = theme(false);
        let rows: [&MergeRequest; 2] = [&wide, &plain];
        let allocation = allocate(&Column::DEFAULT, width, &rows);
        let tab = tab();

        terminal
            .draw(|frame| {
                let table = build(&rows, &tab, &allocation, &theme, now(), false);
                frame.render_widget(table, frame.area());
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let cell_at = |x: u16, y: u16| buffer[(x, y)].symbol().to_owned();

        // Where each row's DIFF column actually starts. Comparing the two rows tests the
        // property directly, without depending on ratatui's internal column spacing.
        let diff_start = |y: u16| (0..width).find(|x| cell_at(*x, y) == "+");

        let wide_row = diff_start(1).expect("wide-title row should render a diff");
        let ascii_row = diff_start(2).expect("ascii row should render a diff");

        assert_eq!(
            wide_row, ascii_row,
            "a wide-character title shifted the columns to its right"
        );
    }

    #[test]
    fn an_empty_table_renders_just_the_header() {
        let lines = render(&[], &tab(), 120, 5);

        assert!(lines[0].contains("TITLE"));
        assert!(
            lines[1].trim().is_empty(),
            "expected a blank row, got {:?}",
            lines[1]
        );
    }

    #[test]
    fn the_ascii_theme_renders_without_unicode() {
        let mut m = mr("a", "jdoe");
        m.pipeline = Some(Pipeline {
            url: "https://example.com/p".into(),
            status: PipelineStatus::Success,
            finished_at: None,
        });

        let mut terminal = Terminal::new(TestBackend::new(120, 4)).unwrap();
        let theme = theme(true);
        let allocation = allocate(&Column::DEFAULT, 120, &[&m]);
        let tab = tab();

        terminal
            .draw(|frame| {
                let table = build(&[&m], &tab, &allocation, &theme, now(), false);
                frame.render_widget(table, frame.area());
            })
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        // The em dash in the APRV column is the one exception; it has no ASCII
        // equivalent that reads as "not applicable".
        let unexpected: Vec<char> = rendered
            .chars()
            .filter(|c| !c.is_ascii() && *c != '—')
            .collect();
        assert!(
            unexpected.is_empty(),
            "non-ASCII in ascii theme: {unexpected:?}"
        );
    }

    /// The allocated widths have to be the widths drawn. When the table asks ratatui's
    /// layout for more cells than the area has, the solver takes them back from whichever
    /// column it likes — and every rendered line is still exactly as wide as the area, so
    /// nothing else here notices.
    #[test]
    fn every_column_renders_at_its_allocated_position_and_width() {
        // A long author and a long repo, so the guard also covers both fitted columns,
        // not just the fixed-width ones.
        let mut m = mr("a", "a-fairly-long-username");
        m.project_name = "a-fairly-long-repo-name".into();
        let rows = [&m];

        for width in [80u16, 120, 200] {
            let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
            let theme = theme(false);
            let allocation = allocate(&Column::DEFAULT, width, &rows);
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 3,
            };

            terminal
                .draw(|frame| {
                    let table = build(&rows, &tab(), &allocation, &theme, now(), false);
                    frame.render_widget(table, area);
                })
                .unwrap();

            let buffer = terminal.backend().buffer();
            let header: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();

            for (column, column_width) in &allocation.widths {
                let x = column_x(&allocation, area, *column).expect("a visible column");

                // `approved` has no header text, so there is nothing for `find` to
                // locate — only that its cells are blank, not some other column's text.
                if !column.header().is_empty() {
                    let drawn = header
                        .find(column.header())
                        .expect("every visible column draws its header");

                    assert_eq!(
                        drawn, x as usize,
                        "at {width}: {column:?} drew at {drawn}, allocated {x}\n{header}"
                    );
                }
                assert_eq!(
                    &header[x as usize..(x + column_width) as usize],
                    pad(column.header(), *column_width as usize),
                    "at {width}: {column:?} did not get its allocated {column_width} cells"
                );
            }
        }
    }

    /// AUTHOR and REPO are both content-fitted, so a one-cell gap can read as part of the
    /// neighbour's text rather than as a boundary. There have to be at least two blank
    /// cells between AUTHOR and REPO, and between REPO and TITLE, at any width where all
    /// three are visible.
    #[test]
    fn author_repo_and_title_are_separated_by_at_least_two_cells() {
        let m = mr("a", "jdoe");
        let rows = [&m];

        for width in [60u16, 80, 100, 120, 200] {
            let allocation = allocate(&Column::DEFAULT, width, &rows);
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 1,
            };

            let (Some(author_x), Some(author_width), Some(repo_x), Some(repo_width), Some(title_x)) = (
                column_x(&allocation, area, Column::Author),
                allocation.width_of(Column::Author),
                column_x(&allocation, area, Column::Repo),
                allocation.width_of(Column::Repo),
                column_x(&allocation, area, Column::Title),
            ) else {
                continue;
            };

            assert!(
                repo_x >= author_x + author_width + 2,
                "at {width}: only {} cells between author and repo",
                repo_x - (author_x + author_width)
            );
            assert!(
                title_x >= repo_x + repo_width + 2,
                "at {width}: only {} cells between repo and title",
                title_x - (repo_x + repo_width)
            );
        }
    }

    /// Render the default columns into a buffer, optionally linked.
    fn buffer_of(
        rows: &[MergeRequest],
        width: u16,
        height: u16,
        links: bool,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = theme(false);
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, width, &refs);
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };

        terminal
            .draw(|frame| {
                let table = build(&refs, &tab(), &allocation, &theme, now(), false);
                frame.render_widget(table, area);
                if links {
                    link_titles(frame.buffer_mut(), area, &refs, &allocation, &theme, now());
                }
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn linked_rows() -> Vec<MergeRequest> {
        ["1", "2"]
            .iter()
            .map(|id| {
                let mut m = mr(id, "jdoe");
                m.title = format!("Merge request {id}");
                m.web_url = format!("https://gitlab.example.com/a/b/-/merge_requests/{id}");
                m
            })
            .collect()
    }

    /// A title becomes clickable, pointing at that row's merge request.
    #[test]
    fn titles_are_emitted_as_hyperlinks_to_their_merge_request() {
        let rows = linked_rows();
        let buffer = buffer_of(&rows, 120, 4, true);
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, 120, &refs);
        let x = column_x(
            &allocation,
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 4,
            },
            Column::Title,
        )
        .unwrap();

        for (index, mr) in rows.iter().enumerate() {
            let symbol = buffer[(x, 1 + index as u16)].symbol();
            assert!(
                symbol.starts_with(&hyperlink::open(&mr.id, &mr.web_url)),
                "row {index} opened {symbol:?}"
            );
        }

        let row: String = (0..120).map(|cx| buffer[(cx, 1)].symbol()).collect();
        assert!(
            row.contains(hyperlink::CLOSE),
            "the link is closed: {row:?}"
        );
    }

    /// Two rows drawn back to back need distinct `id`s, or a terminal is free to treat
    /// them as one link broken across lines (the OSC 8 spec explicitly allows this).
    #[test]
    fn adjacent_rows_open_links_with_different_ids() {
        let rows = linked_rows();
        let buffer = buffer_of(&rows, 120, 4, true);
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, 120, &refs);
        let x = column_x(
            &allocation,
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 4,
            },
            Column::Title,
        )
        .unwrap();

        let first = buffer[(x, 1)].symbol();
        let second = buffer[(x, 2)].symbol();
        assert_ne!(
            first, second,
            "adjacent rows opened identical hyperlinks: {first:?}"
        );
    }

    /// The clickable region stops where the title text ends, not at the far edge of the
    /// (usually much wider) title column: a short title must not drag the rest of its
    /// column's padding into the link.
    #[test]
    fn the_link_covers_only_the_title_text_not_the_columns_padding() {
        let mut rows = linked_rows();
        rows[0].title = "x".to_owned();
        let buffer = buffer_of(&rows, 120, 4, true);
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, 120, &refs);
        let x = column_x(
            &allocation,
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 4,
            },
            Column::Title,
        )
        .unwrap();

        let title_cell = buffer[(x, 1)].symbol();
        assert!(
            title_cell.contains(hyperlink::CLOSE),
            "a one-character title should open and close on that same cell: {title_cell:?}"
        );

        let padding_cell = buffer[(x + 1, 1)].symbol();
        assert_eq!(
            padding_cell, " ",
            "padding right after a short title must not carry the link: {padding_cell:?}"
        );
    }

    /// The escape is additive: every visible cell has to stay exactly where it was.
    #[test]
    fn hyperlinks_shift_nothing_on_screen() {
        let rows = linked_rows();
        let plain = buffer_of(&rows, 120, 4, false);
        let linked = buffer_of(&rows, 120, 4, true);

        let visible = |buffer: &ratatui::buffer::Buffer| -> Vec<String> {
            (0..4)
                .map(|y| {
                    (0..120)
                        .map(|x| buffer[(x, y)].symbol().replace(['\x1b', '\\'], ""))
                        .collect::<String>()
                        .replace(
                            "]8;id=1;https://gitlab.example.com/a/b/-/merge_requests/1",
                            "",
                        )
                        .replace(
                            "]8;id=2;https://gitlab.example.com/a/b/-/merge_requests/2",
                            "",
                        )
                        .replace("]8;;", "")
                })
                .collect()
        };

        assert_eq!(visible(&plain), visible(&linked));
    }

    /// The escapes are zero-width on the terminal but not to `unicode_width`, which is
    /// what ratatui's diff uses to decide how many cells a symbol covers. Unpinned, the
    /// first cell of the link swallows every column to its right on the next redraw.
    #[test]
    fn hyperlink_escapes_do_not_consume_the_columns_after_them() {
        let rows = linked_rows();
        let width = 120u16;
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height: 4,
        };
        let linked = buffer_of(&rows, width, 4, true);
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, width, &refs);

        let blank = ratatui::buffer::Buffer::empty(area);
        let updates = blank.diff(&linked);

        for column in [Column::Pipeline, Column::Updated, Column::Diff] {
            let x = column_x(&allocation, area, column).unwrap();
            assert!(
                updates.iter().any(|(ux, uy, _)| *ux == x && *uy == 1),
                "{column:?} at {x} was skipped by the diff after the link"
            );
        }
    }

    /// A writer that keeps what was written, since `CrosstermBackend` owns its own.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// What the terminal actually receives.
    ///
    /// `TestBackend` keeps a buffer; a real backend is handed only the cells `Terminal`
    /// decided had changed, and that decision is made from the symbol widths the escapes
    /// inflate. This is the path where an unpinned link eats the rest of the row, and the
    /// closest thing to opening the table in a terminal that supports OSC 8.
    #[test]
    fn the_bytes_sent_to_a_real_terminal_carry_the_link_and_the_whole_row() {
        let width = 120u16;
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height: 4,
        };
        let rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, width, &refs);

        let sink = Sink::default();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .unwrap();

        terminal
            .draw(|frame| {
                let table = build(&refs, &tab(), &allocation, &theme(false), now(), false);
                frame.render_widget(table, area);
                link_titles(
                    frame.buffer_mut(),
                    area,
                    &refs,
                    &allocation,
                    &theme(false),
                    now(),
                );
            })
            .unwrap();

        let written = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();

        assert!(
            written.contains(&hyperlink::open(&rows[0].id, &rows[0].web_url)),
            "the link was never written"
        );
        assert!(written.contains(hyperlink::CLOSE), "the link was left open");
        // Checked piecewise because each is written with its own colour escape between.
        for after_the_title in ["+310", "-4", "No"] {
            assert!(
                written.contains(after_the_title),
                "`{after_the_title}` was skipped after the link:\n{written:?}"
            );
        }
    }

    /// A title/link stays identical between two draws, but a column after it (pipeline)
    /// changes — the realistic "live refresh" case, where most of a row's text content is
    /// unchanged and only a small part of it needs to be redrawn. No other test drives a
    /// second, incremental draw against a real backend.
    #[test]
    fn a_second_draw_leaves_an_unchanged_title_and_link_untouched() {
        let width = 120u16;
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height: 4,
        };
        let mut rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, width, &refs);

        let sink = Sink::default();
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(sink.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .unwrap();

        let t = tab();
        terminal
            .draw(|frame| {
                let table = build(&refs, &t, &allocation, &theme(false), now(), false);
                frame.render_widget(table, area);
                link_titles(
                    frame.buffer_mut(),
                    area,
                    &refs,
                    &allocation,
                    &theme(false),
                    now(),
                );
            })
            .unwrap();
        sink.0.lock().unwrap().clear();

        rows[0].pipeline = Some(Pipeline {
            url: "https://example.com/p".into(),
            status: PipelineStatus::Failed,
            finished_at: None,
        });
        let refs2: Vec<&MergeRequest> = rows.iter().collect();
        terminal
            .draw(|frame| {
                let table = build(&refs2, &t, &allocation, &theme(false), now(), false);
                frame.render_widget(table, area);
                link_titles(
                    frame.buffer_mut(),
                    area,
                    &refs2,
                    &allocation,
                    &theme(false),
                    now(),
                );
            })
            .unwrap();

        let written = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();

        assert!(
            !written.contains("Merge request"),
            "title was redrawn even though it did not change: {written:?}"
        );
        assert!(
            written.contains('✘'),
            "the new pipeline glyph must appear: {written:?}"
        );
    }

    /// A title exactly filling its column ends on the trailing half of a wide grapheme,
    /// which carries no symbol — the closing escape has to land somewhere printable.
    #[test]
    fn a_title_of_wide_characters_still_closes_its_link() {
        let mut m = mr("1", "jdoe");
        m.title = "デ".repeat(80);
        let rows = vec![m];

        let buffer = buffer_of(&rows, 120, 3, true);
        let row: String = (0..120).map(|x| buffer[(x, 1)].symbol()).collect();

        assert!(row.contains(hyperlink::CLOSE), "{row:?}");
    }

    #[test]
    fn linking_an_empty_table_or_a_columnless_one_does_nothing() {
        let buffer = buffer_of(&[], 120, 3, true);
        assert!(
            (0..120).all(|x| !buffer[(x, 1)].symbol().contains('\x1b')),
            "no rows, no escapes"
        );

        let narrow = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        };
        let mut empty = ratatui::buffer::Buffer::empty(narrow);
        let rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        link_titles(
            &mut empty,
            narrow,
            &refs,
            &allocate(&[Column::Author], 10, &refs),
            &theme(false),
            now(),
        );
        assert!((0..10).all(|x| !empty[(x, 1)].symbol().contains('\x1b')));
    }

    #[test]
    fn visible_row_count_excludes_the_header() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 25,
        };
        assert_eq!(visible_row_count(area), 24);

        let tiny = Rect { height: 1, ..area };
        assert_eq!(visible_row_count(tiny), 0, "header only");

        let none = Rect { height: 0, ..area };
        assert_eq!(visible_row_count(none), 0);
    }

    /// The windowed slice is exactly the `viewport` rows from `scroll`, clamped at both
    /// ends — build and link_titles draw the same rows only if their inputs agree.
    #[test]
    fn windowed_is_clamped_at_both_ends() {
        let rows: Vec<MergeRequest> = (0..10).map(|i| mr(&format!("id-{i}"), "someone")).collect();

        assert_eq!(windowed(&rows, 2, 4).len(), 4);
        assert_eq!(windowed(&rows, 2, 4)[0].id, "id-2");

        let last_start = windowed(&rows, 8, 4);
        assert_eq!(last_start.len(), 2, "clamped past the end");
        assert_eq!(last_start[0].id, "id-8");
        assert_eq!(last_start[1].id, "id-9");

        assert_eq!(windowed(&rows, 50, 4).len(), 0, "off the end draws nothing");
        assert_eq!(windowed(&rows, 2, 0).len(), 0, "no viewport");
        assert_eq!(windowed::<MergeRequest>(&[], 0, 4).len(), 0);
    }
}
