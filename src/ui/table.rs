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
use std::collections::{HashMap, HashSet};
use std::io;

use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell as BufferCell};
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::state::Tab;
use crate::config::schema::{Column, PeopleDisplay};
use crate::gitlab::model::{MergeRequest, MrState, User};
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
/// and skips the content measurements below.
///
/// `rows` should be the tab's full row set, not the visible window — measuring only what
/// is on screen would make a column resize while scrolling.
pub fn allocate(
    columns: &[Column],
    width: u16,
    rows: &[&MergeRequest],
    modes: PeopleDisplayModes,
) -> Allocation {
    let fitted = columns::Fitted {
        author: author_fit(rows),
        repo: repo_fit(rows),
        assigned: assigned_fit(rows, modes.assigned),
        approver: approver_fit(rows, modes.approver),
        reviewer: reviewer_fit(rows, modes.reviewer),
        merged_by: merged_by_fit(rows, modes.merged_by),
        id: id_fit(rows),
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

/// The `id` column's content width: the widest `!iid` on `rows`, floored at
/// [`columns::ID_MIN`] and capped at [`columns::FIT_MAX`].
fn id_fit(rows: &[&MergeRequest]) -> Option<u16> {
    let widest = rows.iter().map(|mr| mr.iid.width() + 1).max()?;
    let clamped = widest
        .max(columns::ID_MIN as usize)
        .min(columns::FIT_MAX as usize);
    u16::try_from(clamped).ok()
}

/// The ASSIGNED column's content width in `username` display mode: the widest rendered
/// cell on `rows`. `None` in `yes_no` or `trigram` mode, where the column stays the old
/// fixed width regardless of content.
fn assigned_fit(rows: &[&MergeRequest], mode: PeopleDisplay) -> Option<u16> {
    if mode != PeopleDisplay::Username {
        return None;
    }
    content_fit(
        rows.iter()
            .map(|mr| assigned_cell(mr, PeopleDisplay::Username).width()),
        Column::Assigned,
    )
}

/// The APPROVER column's content width: the widest rendered cell on `rows`, in any
/// display mode — `Yes`/`No` and trigram cells simply floor at the header.
fn approver_fit(rows: &[&MergeRequest], mode: PeopleDisplay) -> Option<u16> {
    content_fit(
        rows.iter().map(|mr| approver_cell(mr, mode).width()),
        Column::Approver,
    )
}

/// The REVIEWER column's content width; see [`approver_fit`].
fn reviewer_fit(rows: &[&MergeRequest], mode: PeopleDisplay) -> Option<u16> {
    content_fit(
        rows.iter().map(|mr| reviewer_cell(mr, mode).width()),
        Column::Reviewer,
    )
}

/// The MERGED BY column's content width; see [`approver_fit`].
fn merged_by_fit(rows: &[&MergeRequest], mode: PeopleDisplay) -> Option<u16> {
    content_fit(
        rows.iter().map(|mr| merged_by_cell(mr, mode).width()),
        Column::MergedBy,
    )
}

/// Shared by the `*_fit` helpers: the widest of `widths`, floored at
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

/// No one to name, in a column that names people.
const NOBODY_MARK: &str = "-";

/// The ASSIGNED cell, in whichever of the three [`PeopleDisplay`] modes is configured.
fn assigned_cell(mr: &MergeRequest, mode: PeopleDisplay) -> Cow<'_, str> {
    match mode {
        PeopleDisplay::YesNo => Cow::Borrowed(if mr.assigned_to_me() { "Yes" } else { "No" }),
        PeopleDisplay::Username => {
            people_cell(mr.assignees.iter().map(|user| user.username.as_str()))
        }
        PeopleDisplay::Trigram => match mr.first_assignee() {
            Some(user) => Cow::Owned(trigram(user.name.as_deref(), &user.username)),
            None => Cow::Borrowed(NOBODY_MARK),
        },
    }
}

/// The APPROVER cell, in whichever of the three [`PeopleDisplay`] modes is configured.
fn approver_cell(mr: &MergeRequest, mode: PeopleDisplay) -> Cow<'_, str> {
    match mode {
        PeopleDisplay::YesNo => Cow::Borrowed(if mr.approved_by_me() { "Yes" } else { "No" }),
        PeopleDisplay::Username => people_cell(mr.approved_by.iter().map(String::as_str)),
        PeopleDisplay::Trigram => match mr.approved_by.first() {
            Some(username) => Cow::Owned(trigram(None, username)),
            None => Cow::Borrowed(NOBODY_MARK),
        },
    }
}

/// The REVIEWER cell, in whichever of the three [`PeopleDisplay`] modes is configured.
fn reviewer_cell(mr: &MergeRequest, mode: PeopleDisplay) -> Cow<'_, str> {
    match mode {
        PeopleDisplay::YesNo => Cow::Borrowed(if mr.reviewing_me() { "Yes" } else { "No" }),
        PeopleDisplay::Username => people_cell(mr.reviewers.iter().map(String::as_str)),
        PeopleDisplay::Trigram => match mr.reviewers.first() {
            Some(username) => Cow::Owned(trigram(None, username)),
            None => Cow::Borrowed(NOBODY_MARK),
        },
    }
}

/// The MERGED BY cell, in whichever of the three [`PeopleDisplay`] modes is configured.
/// Blank (`-`) when the author merged it themselves.
fn merged_by_cell(mr: &MergeRequest, mode: PeopleDisplay) -> Cow<'_, str> {
    match mode {
        PeopleDisplay::YesNo => Cow::Borrowed(if mr.merged_by_me() { "Yes" } else { "No" }),
        PeopleDisplay::Username => people_cell(
            mr.merged_by_other()
                .map(|u| u.username.as_str())
                .into_iter(),
        ),
        PeopleDisplay::Trigram => match mr.merged_by_other() {
            Some(user) => Cow::Owned(trigram(user.name.as_deref(), &user.username)),
            None => Cow::Borrowed(NOBODY_MARK),
        },
    }
}

/// `username` display mode, shared by all the people-naming columns: the first
/// person's username, `+N` for the rest, `-` for none.
fn people_cell<'a>(mut people: impl Iterator<Item = &'a str>) -> Cow<'a, str> {
    match people.next() {
        None => Cow::Borrowed(NOBODY_MARK),
        Some(first) => {
            let rest = people.count();
            if rest == 0 {
                Cow::Borrowed(first)
            } else {
                Cow::Owned(format!("{first} +{rest}"))
            }
        }
    }
}

/// Which [`PeopleDisplay`] mode each people-naming column uses, gathered so callers pass
/// one value instead of three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeopleDisplayModes {
    pub assigned: PeopleDisplay,
    pub approver: PeopleDisplay,
    pub reviewer: PeopleDisplay,
    pub merged_by: PeopleDisplay,
}

impl Default for PeopleDisplayModes {
    fn default() -> Self {
        Self {
            assigned: PeopleDisplay::YesNo,
            approver: PeopleDisplay::Username,
            reviewer: PeopleDisplay::Username,
            merged_by: PeopleDisplay::Username,
        }
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
    modes: PeopleDisplayModes,
) -> Cow<'a, str> {
    match column {
        Column::Approved => Cow::Owned(approved_cell(mr, theme)),
        Column::Author => Cow::Borrowed(mr.author.username.as_str()),
        Column::Repo => Cow::Borrowed(mr.project_name.as_str()),
        Column::Id => Cow::Owned(format!("!{}", mr.iid)),
        Column::Title => {
            let mark = discussion_prefix(mr, theme);
            match (mark.is_empty(), mr.draft) {
                (true, false) => Cow::Borrowed(mr.title.as_str()),
                (true, true) => Cow::Owned(format!("[Draft] {}", mr.title)),
                (false, false) => Cow::Owned(format!("{mark}{}", mr.title)),
                (false, true) => Cow::Owned(format!("{mark}[Draft] {}", mr.title)),
            }
        }
        // Filled in by the caller, which has the theme and therefore the glyph set.
        Column::Pipeline => Cow::Borrowed(""),
        Column::Assigned => assigned_cell(mr, modes.assigned),
        Column::Approver => approver_cell(mr, modes.approver),
        Column::Reviewer => reviewer_cell(mr, modes.reviewer),
        Column::MergedBy => merged_by_cell(mr, modes.merged_by),
        Column::Status => Cow::Borrowed(status_label(mr)),
        Column::Age => Cow::Owned(relative_time(mr.created_at, now)),
        Column::Updated => Cow::Owned(relative_time(mr.updated_at, now)),
        Column::Diff => Cow::Owned(diff_cell(mr)),
        Column::Branch => Cow::Borrowed(mr.source_branch.as_str()),
    }
}

/// The STATUS cell: the merge request's state, with an open draft reading `draft`.
const fn status_label(mr: &MergeRequest) -> &'static str {
    match mr.state {
        MrState::Opened if mr.draft => "draft",
        state => state.label(),
    }
}

/// The marker shown before the title while discussions are unresolved, else empty.
const fn discussion_prefix(mr: &MergeRequest, theme: &Theme) -> &'static str {
    if mr.unresolved_discussions > 0 {
        theme.discussion_mark()
    } else {
        ""
    }
}

/// Whether a row is dimmed wholesale.
///
/// Drafts always are. Merged and closed MRs are too, but only next to opened ones
/// (`mixed`): in a list of nothing but finished MRs, dimming them all says nothing.
const fn is_dimmed(mr: &MergeRequest, mixed: bool) -> bool {
    mr.draft || (mixed && mr.is_finished())
}

/// Which role a cell's text takes.
const fn cell_role(mr: &MergeRequest, column: Column, dimmed: bool) -> Role {
    if dimmed {
        // Dimmed rows are present for completeness, not for action, and colouring
        // their pipeline green invites reading them as ready.
        return Role::Dim;
    }
    match column {
        // A conflicted or unmergeable title is a problem, not a failure.
        Column::Title if mr.is_blocked() => Role::Warning,
        Column::Status => match mr.state {
            MrState::Opened => Role::Normal,
            MrState::Merged => Role::Success,
            MrState::Closed => Role::Failure,
            MrState::Locked => Role::Warning,
        },
        Column::Diff => Role::Normal,
        _ => Role::Normal,
    }
}

/// Whether a cell should carry the "this is yours" highlight.
///
/// None of the three counts in `yes_no` mode: the text already says it.
const fn marks_me(mr: &MergeRequest, column: Column, modes: PeopleDisplayModes) -> bool {
    match column {
        Column::Assigned => !matches!(modes.assigned, PeopleDisplay::YesNo) && mr.assigned_to_me(),
        Column::Approver => !matches!(modes.approver, PeopleDisplay::YesNo) && mr.approved_by_me(),
        Column::Reviewer => !matches!(modes.reviewer, PeopleDisplay::YesNo) && mr.reviewing_me(),
        Column::MergedBy => !matches!(modes.merged_by, PeopleDisplay::YesNo) && mr.merged_by_me(),
        Column::Approved
        | Column::Author
        | Column::Repo
        | Column::Id
        | Column::Title
        | Column::Pipeline
        | Column::Status
        | Column::Age
        | Column::Updated
        | Column::Diff
        | Column::Branch => false,
    }
}

/// Whether a cell names someone known not to be able to merge this merge request.
///
/// Only the people-naming cells that show a name count: `yes_no` mode shows no one.
fn names_non_merger(mr: &MergeRequest, column: Column, modes: PeopleDisplayModes) -> bool {
    match column {
        Column::Author => mr.author.cannot_merge(),
        Column::Assigned => {
            !matches!(modes.assigned, PeopleDisplay::YesNo)
                && mr.first_assignee().is_some_and(User::cannot_merge)
        }
        _ => false,
    }
}

/// What the gutter shows for a row.
const fn gutter(theme: &Theme, selected: bool, is_new: bool) -> &str {
    // A new row shares the selection bar's glyph; only its colour tells them apart.
    if selected || is_new {
        theme.selection_marker()
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
#[allow(clippy::too_many_arguments)]
fn row<'a>(
    mr: &MergeRequest,
    allocation: &Allocation,
    theme: &Theme,
    selected: bool,
    is_new: bool,
    dimmed: bool,
    now: jiff::Timestamp,
    modes: PeopleDisplayModes,
) -> Row<'a> {
    let gutter_cell = Cell::from(gutter(theme, selected, is_new).to_owned());
    let fresh = is_new && !selected;
    let gutter_cell = if selected {
        gutter_cell.style(theme.style(Role::Marker))
    } else if fresh {
        gutter_cell.style(theme.style(Role::Fresh))
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
                let role = if dimmed { Role::Dim } else { role };
                Cell::from(pad(glyph, width)).style(theme.style(role))
            }
            // Additions and deletions are coloured separately, so the cell is two spans.
            Column::Diff if !dimmed => diff_spans(mr, width, theme),
            Column::Title if fresh => {
                let text = truncate(
                    &cell_text(mr, *column, theme, now, modes),
                    width,
                    theme.ellipsis(),
                );
                Cell::from(pad(&text, width)).style(theme.style(Role::Fresh))
            }
            // Bold on top of `Role::Success`'s green, so an approved MR stands out at a
            // glance rather than blending into the rest of the row.
            Column::Approved if mr.approved && !dimmed => {
                let text = truncate(
                    &cell_text(mr, *column, theme, now, modes),
                    width,
                    theme.ellipsis(),
                );
                Cell::from(pad(&text, width)).style(theme.emphasise(Role::Success))
            }
            // A name that cannot merge outranks the green "yours" mark: being unable to
            // merge your own MR is the thing to notice.
            other if names_non_merger(mr, *other, modes) && !dimmed => {
                let text = truncate(
                    &cell_text(mr, *other, theme, now, modes),
                    width,
                    theme.ellipsis(),
                );
                Cell::from(pad(&text, width)).style(theme.style(Role::Warning))
            }
            // Green, not bold: bold is APRV's signal, and this only marks the row as
            // yours. ASG carries the same fact `Yes`/`No` did before the trigram existed;
            // APPROVER and REVIEWER extend it to a name the ASG column never had.
            other if marks_me(mr, *other, modes) && !dimmed => {
                let text = truncate(
                    &cell_text(mr, *other, theme, now, modes),
                    width,
                    theme.ellipsis(),
                );
                Cell::from(pad(&text, width)).style(theme.style(Role::Success))
            }
            other => {
                let text = truncate(
                    &cell_text(mr, *other, theme, now, modes),
                    width,
                    theme.ellipsis(),
                );
                Cell::from(pad(&text, width)).style(theme.style(cell_role(mr, *other, dimmed)))
            }
        };
        cells.push(cell);
    }

    let mut row = Row::new(cells);
    if selected {
        row = row.style(theme.style(Role::Selection));
    } else if dimmed {
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
    modes: PeopleDisplayModes,
) -> Table<'a> {
    let selected = tab.selected_id();
    // Over every fetched row, not the visible ones: a search must not toggle dimming.
    let mixed = tab.all().iter().any(|mr| mr.state == MrState::Opened);

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
                is_dimmed(mr, mixed),
                now,
                modes,
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

/// The exact text [`row`] draws in a `column` cell, before padding.
///
/// [`link_titles`] needs this again: the clickable region has to stop where the visible
/// text does, not run on into the padding that fills out the rest of the column.
fn linked_text(
    mr: &MergeRequest,
    column: Column,
    width: usize,
    theme: &Theme,
    now: jiff::Timestamp,
) -> String {
    // The display modes only affect the people-naming columns, so their value here is
    // moot.
    truncate(
        &cell_text(mr, column, theme, now, PeopleDisplayModes::default()),
        width,
        theme.ellipsis(),
    )
}

/// One row's linked text: the cells to reprint inside a single OSC 8 pair.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSpan {
    open: String,
    /// Wide graphemes' trailing halves are left out, as ratatui's own diff does.
    cells: Vec<(u16, u16, BufferCell)>,
}

/// Capture each row's `column` cell (`title` or `id`, per `[ui].link`) as a link to its
/// merge request, for [`Links`] to print once the frame is drawn.
///
/// The escapes never enter the buffer: ratatui resends only the cells that changed, so a
/// link split across cells can be resent without its close, or leave an unchanged cell
/// pointing at the previous row's merge request.
///
/// Call only when the terminal advertises OSC 8, and not while a popup is on screen —
/// the spans are reprinted after the frame and would land on top of it.
pub fn link_spans(
    buffer: &Buffer,
    area: Rect,
    rows: &[&MergeRequest],
    allocation: &Allocation,
    column: Column,
    theme: &Theme,
    now: jiff::Timestamp,
) -> Vec<LinkSpan> {
    let (Some(x), Some(width)) = (
        column_x(allocation, area, column),
        allocation.width_of(column),
    ) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    for (index, mr) in rows.iter().copied().enumerate() {
        // The header takes the first row of the area.
        let Ok(offset) = u16::try_from(index + 1) else {
            break;
        };
        if offset >= area.height {
            break;
        }
        let y = area.y + offset;

        let text = linked_text(mr, column, width as usize, theme, now);
        let Ok(text_width) = u16::try_from(text.width()) else {
            continue;
        };
        if text_width == 0 {
            continue;
        }

        let end = x + text_width;
        let mut cells = Vec::new();
        // The discussion marker is a status, not part of the title: leave it unlinked.
        let skip = if column == Column::Title {
            u16::try_from(discussion_prefix(mr, theme).width()).unwrap_or(0)
        } else {
            0
        };
        let mut cell_x = x + skip.min(text_width);
        while cell_x < end {
            let Some(cell) = buffer.cell((cell_x, y)) else {
                break;
            };
            cells.push((cell_x, y, cell.clone()));
            cell_x += u16::try_from(cell.symbol().width().max(1)).unwrap_or(1);
        }
        spans.push(LinkSpan {
            open: hyperlink::open(&mr.id, &mr.web_url),
            cells,
        });
    }
    spans
}

/// The links on the terminal, reprinted over ratatui's output only where they changed.
#[derive(Debug, Default)]
pub struct Links {
    area: Rect,
    drawn: Vec<LinkSpan>,
}

impl Links {
    /// Write the `spans` of the frame just drawn into `buffer`, after `Terminal::draw`.
    ///
    /// A span identical to last frame's is skipped: ratatui did not resend its cells
    /// either, so the terminal still holds them linked. Cells that were linked last frame
    /// and are no longer are reprinted plain from `buffer`, or they would keep their old
    /// link even where their glyph did not change.
    pub fn write<B>(
        &mut self,
        backend: &mut B,
        buffer: &Buffer,
        spans: Vec<LinkSpan>,
    ) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + io::Write,
    {
        // A resize clears the screen, links included.
        if buffer.area != self.area {
            self.area = buffer.area;
            self.drawn.clear();
        }

        // Spans are matched by where they start and what they open, so "unchanged since
        // last frame" is a lookup, not a scan of every span.
        let key = |span: &LinkSpan| {
            let (x, y) = span.cells.first().map_or((0, 0), |&(x, y, _)| (x, y));
            (x, y, span.open.clone())
        };
        let drawn: HashMap<_, &LinkSpan> = self.drawn.iter().map(|s| (key(s), s)).collect();
        let current: HashMap<_, &LinkSpan> = spans.iter().map(|s| (key(s), s)).collect();

        let fresh: Vec<&LinkSpan> = spans
            .iter()
            .filter(|span| drawn.get(&key(span)) != Some(span))
            .collect();
        let covered: HashSet<(u16, u16)> = fresh
            .iter()
            .flat_map(|span| span.cells.iter().map(|&(x, y, _)| (x, y)))
            .collect();
        let stale: Vec<(u16, u16)> = self
            .drawn
            .iter()
            .filter(|span| current.get(&key(span)) != Some(span))
            .flat_map(|span| span.cells.iter().map(|&(x, y, _)| (x, y)))
            .filter(|cell| !covered.contains(cell))
            // A trailing half of a wide glyph now drawn there: printing it would blank
            // the glyph's right half.
            .filter(|&(x, y)| {
                x == buffer.area.x
                    || buffer
                        .cell((x - 1, y))
                        .is_none_or(|left| left.symbol().width() < 2)
            })
            .collect();

        if fresh.is_empty() && stale.is_empty() {
            self.drawn = spans;
            return Ok(());
        }

        // A failure partway must not leave the terminal inside a link, or the next frame's
        // plain cells would join it; and `drawn` no longer describes the screen, so the
        // next call reprints everything.
        let printed = (|| {
            backend.draw(
                stale
                    .iter()
                    .filter_map(|&(x, y)| buffer.cell((x, y)).map(|c| (x, y, c))),
            )?;
            for span in &fresh {
                backend.write_all(span.open.as_bytes())?;
                backend.draw(span.cells.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
                backend.write_all(hyperlink::CLOSE.as_bytes())?;
            }
            Backend::flush(backend)?;
            Ok(())
        })();
        if printed.is_err() {
            let _ = backend.write_all(hyperlink::CLOSE.as_bytes());
            let _ = Backend::flush(backend);
            self.drawn.clear();
            return printed;
        }

        self.drawn = spans;
        Ok(())
    }
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
    use crate::ui::screen::{Screen, Sink};
    use ratatui::backend::{CrosstermBackend, TestBackend};
    use ratatui::{Terminal, TerminalOptions, Viewport};

    fn now() -> jiff::Timestamp {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn all(mode: PeopleDisplay) -> PeopleDisplayModes {
        PeopleDisplayModes {
            assigned: mode,
            approver: mode,
            reviewer: mode,
            merged_by: mode,
        }
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
        let allocation = allocate(&Column::DEFAULT, width, &refs, all(PeopleDisplay::YesNo));

        terminal
            .draw(|frame| {
                let table = build(
                    &refs,
                    tab,
                    &allocation,
                    &theme,
                    now(),
                    PeopleDisplayModes::default(),
                );
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

        let narrow_author = allocate(&Column::DEFAULT, 120, &[&short], all(PeopleDisplay::YesNo));
        let wide_author = allocate(&Column::DEFAULT, 120, &[&long], all(PeopleDisplay::YesNo));

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

        let narrow_repo = allocate(&Column::DEFAULT, 120, &[&short], all(PeopleDisplay::YesNo));
        let wide_repo = allocate(&Column::DEFAULT, 120, &[&long], all(PeopleDisplay::YesNo));

        let narrow_title = narrow_repo.width_of(Column::Title).unwrap();
        let wide_title = wide_repo.width_of(Column::Title).unwrap();

        assert!(
            narrow_title > wide_title,
            "a short repo ({narrow_title}) should leave more room for the title than a long one ({wide_title})"
        );
    }

    /// `assigned_fit` only measures in `username` display mode — `yes_no` and `trigram`
    /// keep the column at its old fixed width regardless of content.
    #[test]
    fn assigned_fit_only_measures_in_username_mode() {
        let mut m = mr("a", "someone");
        m.assignees = vec![User::new("a-fairly-long-username")];

        assert_eq!(
            assigned_fit(&[&m], PeopleDisplay::Username),
            Some("a-fairly-long-username".width() as u16)
        );
        assert_eq!(assigned_fit(&[&m], PeopleDisplay::YesNo), None);
        assert_eq!(assigned_fit(&[&m], PeopleDisplay::Trigram), None);
    }

    /// In `username` mode, a wide assignee widens the ASSIGNED column exactly as a wide
    /// author or repo would — and gives the title back the room it doesn't need.
    #[test]
    fn a_long_assignee_username_widens_the_assigned_column() {
        let mut short = mr("a", "someone");
        short.assignees = vec![User::new("ab")];
        let mut long = mr("b", "someone");
        long.assignees = vec![User::new("a-fairly-long-username")];

        let narrow_assigned = allocate(
            &Column::DEFAULT,
            200,
            &[&short],
            all(PeopleDisplay::Username),
        );
        let wide_assigned = allocate(
            &Column::DEFAULT,
            200,
            &[&long],
            all(PeopleDisplay::Username),
        );

        // Floored at the ASG header's own width (3), not the old fixed 4 — that fixed
        // width was never about the header, only about fitting `Yes`/`No`/a trigram.
        assert_eq!(narrow_assigned.width_of(Column::Assigned), Some(3));
        assert_eq!(
            wide_assigned.width_of(Column::Assigned),
            Some("a-fairly-long-username".width() as u16)
        );
    }

    #[test]
    fn approver_and_reviewer_fit_measure_the_rendered_cell() {
        let mut m = mr("a", "someone");
        m.approved_by = vec!["alice-anderson".to_owned(), "bob".to_owned()];
        m.reviewers = vec!["carol-the-reviewer".to_owned()];

        assert_eq!(
            approver_fit(&[&m], PeopleDisplay::Username),
            Some("alice-anderson +1".width() as u16)
        );
        assert_eq!(
            reviewer_fit(&[&m], PeopleDisplay::Username),
            Some("carol-the-reviewer".width() as u16)
        );
    }

    #[test]
    fn approver_and_reviewer_fit_floor_at_the_header_and_cap_at_the_maximum() {
        let mut short = mr("a", "someone");
        short.approved_by = vec!["al".to_owned()];
        short.reviewers = vec!["al".to_owned()];
        let mut long = mr("b", "someone");
        long.approved_by = vec!["x".repeat(50)];
        long.reviewers = vec!["x".repeat(50)];

        let header = u16::try_from(Column::Approver.header().width()).unwrap();
        assert_eq!(
            approver_fit(&[&short], PeopleDisplay::Username),
            Some(header)
        );
        assert_eq!(approver_fit(&[&short], PeopleDisplay::YesNo), Some(header));
        assert_eq!(
            approver_fit(&[&long], PeopleDisplay::Username),
            Some(columns::FIT_MAX)
        );
        assert_eq!(
            reviewer_fit(&[&long], PeopleDisplay::Username),
            Some(columns::FIT_MAX)
        );
        assert_eq!(approver_fit(&[], PeopleDisplay::Username), None);
    }

    #[test]
    fn a_long_approver_username_widens_the_approver_and_narrows_the_title() {
        let mut short = mr("a", "someone");
        short.approved_by = vec!["al".to_owned()];
        let mut long = mr("b", "someone");
        long.approved_by = vec!["a-fairly-long-username".to_owned()];

        let modes = all(PeopleDisplay::Username);
        let narrow = allocate(&Column::DEFAULT, 200, &[&short], modes);
        let wide = allocate(&Column::DEFAULT, 200, &[&long], modes);

        assert_eq!(narrow.width_of(Column::Approver), Some(8));
        assert_eq!(
            wide.width_of(Column::Approver),
            Some("a-fairly-long-username".width() as u16)
        );
        assert!(wide.width_of(Column::Title) < narrow.width_of(Column::Title));
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
            let text = cell_text(&m, column, &ascii, now(), PeopleDisplayModes::default());
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
            cell_text(
                &m,
                Column::Author,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "jdoe"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Repo,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "web-app"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Title,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "Add dark mode"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Assigned,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "Yes"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Diff,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "+310 -4"
        );

        m.assignees.clear();
        m.recompute_derived("me");
        assert_eq!(
            cell_text(
                &m,
                Column::Assigned,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "No"
        );
    }

    /// `-` with nobody, the bare username with one, `name +N` with more.
    #[test]
    fn people_cell_names_the_first_and_counts_the_rest() {
        assert_eq!(people_cell(std::iter::empty::<&str>()), "-");
        assert_eq!(people_cell(["jdoe"].into_iter()), "jdoe");
        assert_eq!(
            people_cell(["jdoe", "bwayne", "asmith"].into_iter()),
            "jdoe +2"
        );
    }

    #[test]
    fn id_fit_follows_the_longest_id_with_a_floor_of_three() {
        let mut short = mr("a", "jdoe");
        short.iid = "7".to_owned();
        let mut long = mr("b", "jdoe");
        long.iid = "12345".to_owned();

        assert_eq!(id_fit(&[&short]), Some(3));
        assert_eq!(id_fit(&[&short, &long]), Some(6));
        assert_eq!(id_fit(&[]), None);
    }

    #[test]
    fn id_cell_shows_the_project_number() {
        let mut m = mr("a", "jdoe");
        m.iid = "3658".to_owned();

        assert_eq!(
            cell_text(
                &m,
                Column::Id,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "!3658"
        );
    }

    #[test]
    fn status_cell_reads_draft_for_an_open_draft_and_the_state_otherwise() {
        let mut m = mr("a", "jdoe");
        let modes = PeopleDisplayModes::default();
        let status = |m: &MergeRequest| {
            cell_text(m, Column::Status, &theme(false), now(), modes).into_owned()
        };
        assert_eq!(status(&m), "open");
        m.draft = true;
        assert_eq!(status(&m), "draft");
        m.state = MrState::Merged;
        assert_eq!(status(&m), "merged");
        m.state = MrState::Closed;
        assert_eq!(status(&m), "closed");
        assert_eq!(cell_role(&m, Column::Status, false), Role::Failure);
    }

    #[test]
    fn merged_by_cell_names_the_merger_and_marks_the_current_user() {
        let mut m = mr("a", "jdoe");
        let default = PeopleDisplayModes::default();
        assert_eq!(
            cell_text(&m, Column::MergedBy, &theme(false), now(), default),
            "-"
        );

        m.merged_by = Some(User::new("asmith"));
        m.recompute_derived("me");
        assert_eq!(
            cell_text(&m, Column::MergedBy, &theme(false), now(), default),
            "asmith"
        );
        assert!(!marks_me(&m, Column::MergedBy, default));

        let yes_no = PeopleDisplayModes {
            merged_by: PeopleDisplay::YesNo,
            ..default
        };
        assert_eq!(
            cell_text(&m, Column::MergedBy, &theme(false), now(), yes_no),
            "No"
        );

        m.recompute_derived("asmith");
        assert!(marks_me(&m, Column::MergedBy, default));
        assert!(!marks_me(&m, Column::MergedBy, yes_no));

        m.merged_by = Some(User::new("JDoe"));
        m.recompute_derived("jdoe");
        assert_eq!(
            cell_text(&m, Column::MergedBy, &theme(false), now(), default),
            "-",
            "a self-merge is left blank"
        );
        assert_eq!(
            cell_text(&m, Column::MergedBy, &theme(false), now(), yes_no),
            "No"
        );
        assert!(!marks_me(&m, Column::MergedBy, default));
    }

    #[test]
    fn approver_and_reviewer_cells_read_from_their_own_lists() {
        let mut m = mr("a", "jdoe");
        m.approved_by = vec!["asmith".to_owned()];
        m.reviewers = vec!["bwayne".to_owned(), "cdavis".to_owned()];

        assert_eq!(
            cell_text(
                &m,
                Column::Approver,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "asmith"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Reviewer,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "bwayne +1"
        );

        m.approved_by.clear();
        m.reviewers.clear();
        assert_eq!(
            cell_text(
                &m,
                Column::Approver,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "-"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Reviewer,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "-"
        );
    }

    /// APPROVER and REVIEWER in `yes_no` mode read the personal flag, not the list;
    /// in `trigram` mode they use the first username — there is no display name for an
    /// approver or reviewer, unlike an assignee.
    #[test]
    fn approver_and_reviewer_cells_support_yes_no_and_trigram_too() {
        let mut m = mr("a", "jdoe");
        m.approved_by = vec!["me".to_owned()];
        m.reviewers = vec!["bwayne".to_owned()];
        m.recompute_derived("me");

        let yes_no = PeopleDisplayModes {
            assigned: PeopleDisplay::YesNo,
            approver: PeopleDisplay::YesNo,
            reviewer: PeopleDisplay::YesNo,
            merged_by: PeopleDisplay::Username,
        };
        assert_eq!(
            cell_text(&m, Column::Approver, &theme(false), now(), yes_no),
            "Yes"
        );
        assert_eq!(
            cell_text(&m, Column::Reviewer, &theme(false), now(), yes_no),
            "No"
        );

        let trigram = PeopleDisplayModes {
            assigned: PeopleDisplay::YesNo,
            approver: PeopleDisplay::Trigram,
            reviewer: PeopleDisplay::Trigram,
            merged_by: PeopleDisplay::Username,
        };
        assert_eq!(
            cell_text(&m, Column::Approver, &theme(false), now(), trigram),
            "ME"
        );
        assert_eq!(
            cell_text(&m, Column::Reviewer, &theme(false), now(), trigram),
            "BWA"
        );

        m.approved_by.clear();
        assert_eq!(
            cell_text(&m, Column::Approver, &theme(false), now(), trigram),
            "-"
        );
    }

    /// The "this is yours" highlight: none of the three counts in `yes_no` mode, since
    /// the text already says it; `username`/`trigram` mode marks whoever is in the list,
    /// not just whoever is shown first.
    #[test]
    fn marks_me_reflects_the_current_user_in_each_column() {
        let mut m = mr("a", "jdoe");
        m.approved_by = vec!["asmith".to_owned(), "me".to_owned()];
        m.reviewers = vec!["bwayne".to_owned()];
        m.assignees = vec![User::new("me")];
        m.recompute_derived("me");

        let yes_no = PeopleDisplayModes {
            assigned: PeopleDisplay::YesNo,
            approver: PeopleDisplay::YesNo,
            reviewer: PeopleDisplay::YesNo,
            merged_by: PeopleDisplay::Username,
        };
        assert!(
            !marks_me(&m, Column::Approver, yes_no),
            "yes_no already says so"
        );
        assert!(
            !marks_me(&m, Column::Reviewer, yes_no),
            "yes_no already says so"
        );
        assert!(
            !marks_me(&m, Column::Assigned, yes_no),
            "yes_no already says so"
        );

        let named = PeopleDisplayModes {
            assigned: PeopleDisplay::Trigram,
            approver: PeopleDisplay::Username,
            reviewer: PeopleDisplay::Username,
            merged_by: PeopleDisplay::Username,
        };
        assert!(marks_me(&m, Column::Approver, named), "me is in the list");
        assert!(
            !marks_me(&m, Column::Reviewer, named),
            "me is not reviewing"
        );
        assert!(marks_me(&m, Column::Assigned, named), "me is the assignee");
        assert!(!marks_me(&m, Column::Title, named), "title never marks");
    }

    #[test]
    fn drafts_are_prefixed_in_the_title() {
        let mut m = mr("a", "someone");
        m.draft = true;
        m.title = "Migration guide".into();

        assert_eq!(
            cell_text(
                &m,
                Column::Title,
                &theme(false),
                now(),
                PeopleDisplayModes::default()
            ),
            "[Draft] Migration guide"
        );
        assert_eq!(
            cell_role(&m, Column::Title, is_dimmed(&m, false)),
            Role::Dim
        );
    }

    #[test]
    fn unresolved_discussions_put_a_marker_before_the_title() {
        let title = |m: &MergeRequest, ascii: bool| {
            cell_text(
                m,
                Column::Title,
                &theme(ascii),
                now(),
                PeopleDisplayModes::default(),
            )
            .into_owned()
        };
        let mut m = mr("a", "someone");
        m.title = "Migration guide".into();

        m.unresolved_discussions = 0;
        assert_eq!(title(&m, false), "Migration guide");

        m.unresolved_discussions = 3;
        assert_eq!(title(&m, false), "💬 Migration guide");
        assert_eq!(title(&m, true), "* Migration guide");

        m.draft = true;
        assert_eq!(title(&m, false), "💬 [Draft] Migration guide");
    }

    #[test]
    fn the_discussion_marker_is_not_part_of_the_link() {
        let mut rows = linked_rows();
        rows[0].title = "Plain".to_owned();
        rows[0].unresolved_discussions = 1;
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 4,
        };
        let buffer = Buffer::empty(area);
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(&Column::DEFAULT, 120, &refs, all(PeopleDisplay::YesNo));
        let x = column_x(&allocation, area, Column::Title).unwrap();
        let spans = link_spans(
            &buffer,
            area,
            &refs,
            &allocation,
            Column::Title,
            &theme(false),
            now(),
        );

        let (first_x, _, _) = spans[0].cells[0];
        assert_eq!(
            first_x,
            x + 3,
            "the link starts after the marker and its space"
        );
        let (plain_x, _, _) = spans[1].cells[0];
        assert_eq!(plain_x, x, "a row without discussions links from the start");
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

    /// The ASG cell in all three display modes: `Yes`/`No` by default, the first
    /// assignee's username (or `-`) in `username` mode, and their trigram (or `-`) in
    /// `trigram` mode.
    #[test]
    fn the_assigned_column_has_three_modes() {
        let mut m = mr("a", "jdoe");
        m.assignees = vec![User {
            username: "cbillow".to_owned(),
            name: Some("Charles Billow".to_owned()),
            can_merge: None,
        }];
        m.recompute_derived("nobody");

        let modes = |assigned| PeopleDisplayModes {
            assigned,
            ..PeopleDisplayModes::default()
        };

        assert_eq!(
            cell_text(
                &m,
                Column::Assigned,
                &theme(false),
                now(),
                modes(PeopleDisplay::Trigram)
            ),
            "CBI"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Assigned,
                &theme(false),
                now(),
                modes(PeopleDisplay::Username)
            ),
            "cbillow"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Assigned,
                &theme(false),
                now(),
                modes(PeopleDisplay::YesNo)
            ),
            "No",
            "yes_no ignores who the assignee is"
        );

        m.assignees.clear();
        assert_eq!(
            cell_text(
                &m,
                Column::Assigned,
                &theme(false),
                now(),
                modes(PeopleDisplay::Trigram)
            ),
            "-",
            "no assignee at all"
        );
        assert_eq!(
            cell_text(
                &m,
                Column::Assigned,
                &theme(false),
                now(),
                modes(PeopleDisplay::Username)
            ),
            "-",
            "no assignee at all"
        );
    }

    /// A conflicted title is a problem, not a failure.
    #[test]
    fn a_blocked_title_takes_the_warning_role() {
        let mut m = mr("a", "someone");
        m.conflicts = true;

        assert_eq!(cell_role(&m, Column::Title, false), Role::Warning);
        assert_eq!(cell_role(&m, Column::Author, false), Role::Normal);
    }

    /// A name that cannot merge is flagged, and the flag beats the green "yours" mark.
    #[test]
    fn a_non_merging_author_or_assignee_is_flagged() {
        let mut m = mr("a", "someone");
        let names = all(PeopleDisplay::Username);
        assert!(!names_non_merger(&m, Column::Author, names), "unknown");

        m.author.can_merge = Some(true);
        assert!(!names_non_merger(&m, Column::Author, names));

        m.author.can_merge = Some(false);
        m.assignees = vec![User {
            can_merge: Some(false),
            ..User::new("me")
        }];
        assert!(names_non_merger(&m, Column::Author, names));
        assert!(names_non_merger(&m, Column::Assigned, names));
        assert!(
            !names_non_merger(&m, Column::Assigned, all(PeopleDisplay::YesNo)),
            "yes/no names no one"
        );
        assert!(!names_non_merger(&m, Column::Title, names));
    }

    /// Colouring a draft's pipeline green invites reading it as ready.
    #[test]
    fn a_draft_dims_every_cell_including_the_pipeline() {
        let mut m = mr("a", "someone");
        m.draft = true;
        m.conflicts = true;

        assert_eq!(
            cell_role(&m, Column::Title, is_dimmed(&m, false)),
            Role::Dim,
            "dim wins over warning"
        );
        assert_eq!(
            cell_role(&m, Column::Author, is_dimmed(&m, false)),
            Role::Dim
        );
    }

    /// Finished MRs recede next to opened ones, but not in a list of only finished ones.
    #[test]
    fn merged_and_closed_are_dimmed_only_next_to_opened_ones() {
        let mut merged = mr("a", "someone");
        merged.state = MrState::Merged;
        let mut closed = mr("b", "someone");
        closed.state = MrState::Closed;
        let mut locked = mr("c", "someone");
        locked.state = MrState::Locked;
        let mut opened = mr("d", "someone");
        opened.state = MrState::Opened;

        assert!(is_dimmed(&merged, true));
        assert!(is_dimmed(&closed, true));
        assert!(!is_dimmed(&opened, true));
        assert!(!is_dimmed(&locked, true), "locked is not finished");
        assert!(
            !is_dimmed(&merged, false),
            "nothing opened to contrast with"
        );
        assert!(!is_dimmed(&closed, false));
    }

    /// Reverse video plus a marker, so selection survives a terminal with
    /// weak reverse video.
    #[test]
    fn the_gutter_distinguishes_selected_new_and_ordinary_rows() {
        let theme = theme(false);

        assert_eq!(gutter(&theme, true, false), theme.selection_marker());
        assert_eq!(gutter(&theme, false, true), theme.selection_marker());
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

    fn fresh_buffer(select: Option<&str>) -> (ratatui::buffer::Buffer, Allocation) {
        let mut approved = mr("gid://1", "jdoe");
        approved.approved = true;
        let mut tab = tab();
        tab.apply_rows(vec![mr("gid://0", "x")], std::time::Instant::now());
        tab.apply_rows(
            vec![mr("gid://0", "x"), approved.clone()],
            std::time::Instant::now(),
        );
        tab.select(select.map(str::to_owned));

        let theme = theme(false);
        let refs = [&approved];
        let allocation = allocate(&Column::DEFAULT, 120, &refs, all(PeopleDisplay::YesNo));
        let area = Rect::new(0, 0, 120, 3);
        let mut terminal = Terminal::new(TestBackend::new(120, 3)).unwrap();
        terminal
            .draw(|frame| {
                let table = build(
                    &refs,
                    &tab,
                    &allocation,
                    &theme,
                    now(),
                    PeopleDisplayModes::default(),
                );
                frame.render_widget(table, area);
            })
            .unwrap();
        (terminal.backend().buffer().clone(), allocation)
    }

    #[test]
    fn a_new_row_draws_a_fresh_gutter_and_a_fresh_title() {
        let (buffer, allocation) = fresh_buffer(None);
        let theme = theme(false);
        let fresh = theme.style(Role::Fresh);
        let area = Rect::new(0, 0, 120, 3);
        let title_x = column_x(&allocation, area, Column::Title).unwrap();
        let approved_x = column_x(&allocation, area, Column::Approved).unwrap();

        assert_eq!(buffer[(0, 1)].symbol(), theme.selection_marker(), "gutter");
        assert_eq!(Some(buffer[(0, 1)].fg), fresh.fg, "gutter is fresh");
        assert_ne!(
            Some(buffer[(approved_x, 1)].fg),
            fresh.fg,
            "approval keeps its own styling"
        );
        assert_eq!(Some(buffer[(title_x, 1)].fg), fresh.fg, "title is fresh");
    }

    #[test]
    fn a_new_row_under_the_cursor_drops_the_fresh_styling() {
        let (buffer, allocation) = fresh_buffer(Some("gid://1"));
        let theme = theme(false);
        let area = Rect::new(0, 0, 120, 3);
        let title_x = column_x(&allocation, area, Column::Title).unwrap();

        assert_eq!(buffer[(0, 1)].symbol(), theme.selection_marker());
        assert_ne!(Some(buffer[(title_x, 1)].fg), theme.style(Role::Fresh).fg);
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
        assert!(
            lines[1].starts_with(theme(false).selection_marker()),
            "row: {:?}",
            lines[1]
        );

        // The marker lasts one refresh cycle: the next fetch brings nothing new, so it
        // goes (mrq-zv4.3).
        tab.apply_rows(
            vec![mr("gid://0", "x"), m.clone()],
            std::time::Instant::now(),
        );
        let lines = render(&[m], &tab, 120, 5);
        assert!(
            !lines[1].starts_with(theme(false).selection_marker()),
            "row: {:?}",
            lines[1]
        );
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
        let allocation = allocate(&Column::DEFAULT, width, &rows, all(PeopleDisplay::YesNo));
        let tab = tab();

        terminal
            .draw(|frame| {
                let table = build(
                    &rows,
                    &tab,
                    &allocation,
                    &theme,
                    now(),
                    PeopleDisplayModes::default(),
                );
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
        let allocation = allocate(&Column::DEFAULT, 120, &[&m], all(PeopleDisplay::YesNo));
        let tab = tab();

        terminal
            .draw(|frame| {
                let table = build(
                    &[&m],
                    &tab,
                    &allocation,
                    &theme,
                    now(),
                    PeopleDisplayModes::default(),
                );
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
            let allocation = allocate(&Column::DEFAULT, width, &rows, all(PeopleDisplay::YesNo));
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 3,
            };

            terminal
                .draw(|frame| {
                    let table = build(
                        &rows,
                        &tab(),
                        &allocation,
                        &theme,
                        now(),
                        PeopleDisplayModes::default(),
                    );
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
            let allocation = allocate(&Column::DEFAULT, width, &rows, all(PeopleDisplay::YesNo));
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

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 4,
    };

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

    /// A real backend, the links printed over it, and what a terminal makes of the bytes.
    struct Linked {
        terminal: Terminal<CrosstermBackend<Sink>>,
        sink: Sink,
        links: Links,
        screen: Screen,
        allocation: Allocation,
    }

    impl Linked {
        fn new(rows: &[MergeRequest]) -> Self {
            let refs: Vec<&MergeRequest> = rows.iter().collect();
            let sink = Sink::default();
            Self {
                terminal: Terminal::with_options(
                    CrosstermBackend::new(sink.clone()),
                    TerminalOptions {
                        viewport: Viewport::Fixed(AREA),
                    },
                )
                .unwrap(),
                sink,
                links: Links::default(),
                screen: Screen::default(),
                allocation: allocate(
                    &Column::DEFAULT,
                    AREA.width,
                    &refs,
                    all(PeopleDisplay::YesNo),
                ),
            }
        }

        /// Draw `rows` with `column` linked, as `App::draw` does, and return the bytes.
        fn draw(&mut self, rows: &[&MergeRequest], column: Column) -> String {
            let t = tab();
            let mut spans = Vec::new();
            let completed = self
                .terminal
                .draw(|frame| {
                    let table = build(
                        rows,
                        &t,
                        &self.allocation,
                        &theme(false),
                        now(),
                        PeopleDisplayModes::default(),
                    );
                    frame.render_widget(table, AREA);
                    spans = link_spans(
                        frame.buffer_mut(),
                        AREA,
                        rows,
                        &self.allocation,
                        column,
                        &theme(false),
                        now(),
                    );
                })
                .unwrap();
            let buffer = completed.buffer.clone();
            self.links
                .write(self.terminal.backend_mut(), &buffer, spans)
                .unwrap();

            let written = self.sink.take();
            self.screen.feed(&written);
            written
        }

        fn x_of(&self, column: Column) -> u16 {
            column_x(&self.allocation, AREA, column).unwrap()
        }
    }

    /// Every cell of `title` on row `y`, and nothing else on it, links to `url`.
    fn assert_row_linked(screen: &Screen, y: u16, x: u16, title: &str, url: &str) {
        let expected: Vec<(u16, &str)> =
            (x..x + title.width() as u16).map(|cx| (cx, url)).collect();
        assert_eq!(screen.linked_in_row(y), expected, "row {y}");
    }

    /// With `[ui].link = "id"` the ID cell carries the link and the title carries none.
    #[test]
    fn the_link_can_sit_on_the_id_column_instead_of_the_title() {
        let rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let mut linked = Linked::new(&rows);
        linked.draw(&refs, Column::Id);

        let id_x = linked.x_of(Column::Id);
        let title_x = linked.x_of(Column::Title);
        assert_eq!(
            linked.screen.link_at(id_x, 1),
            Some(rows[0].web_url.as_str())
        );
        assert_eq!(linked.screen.link_at(title_x, 1), None);
    }

    /// Every cell of a title, and only the title, links to that row's merge request.
    #[test]
    fn titles_are_emitted_as_hyperlinks_to_their_merge_request() {
        let rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let mut linked = Linked::new(&rows);
        linked.draw(&refs, Column::Title);

        let x = linked.x_of(Column::Title);
        for (index, mr) in rows.iter().enumerate() {
            assert_row_linked(&linked.screen, 1 + index as u16, x, &mr.title, &mr.web_url);
        }
        assert!(!linked.screen.link_open());
    }

    /// Two rows drawn back to back need distinct `id`s, or a terminal is free to treat
    /// them as one link broken across lines (the OSC 8 spec explicitly allows this).
    #[test]
    fn adjacent_rows_open_links_with_different_ids() {
        let rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let written = Linked::new(&rows).draw(&refs, Column::Title);

        for mr in &rows {
            assert!(written.contains(&hyperlink::open(&mr.id, &mr.web_url)));
        }
    }

    /// A big window over many rows, drawn the way `App::draw` does, with a terminal replay.
    struct Big {
        area: Rect,
        terminal: Terminal<CrosstermBackend<Sink>>,
        sink: Sink,
        links: Links,
        screen: Screen,
        rows: Vec<MergeRequest>,
        allocation: Allocation,
    }

    impl Big {
        fn new(count: usize, title: impl Fn(usize) -> String) -> Self {
            let area = Rect {
                x: 0,
                y: 0,
                width: 200,
                height: 30,
            };
            let rows: Vec<MergeRequest> = (0..count)
                .map(|i| {
                    let mut m = mr(&i.to_string(), "jdoe");
                    m.title = title(i);
                    m.web_url =
                        format!("https://gitlab.example.com/group/project/-/merge_requests/{i}");
                    m
                })
                .collect();
            let refs: Vec<&MergeRequest> = rows.iter().collect();
            let allocation = allocate(
                &Column::DEFAULT,
                area.width,
                &refs,
                all(PeopleDisplay::YesNo),
            );
            let sink = Sink::default();
            Self {
                area,
                terminal: Terminal::with_options(
                    CrosstermBackend::new(sink.clone()),
                    TerminalOptions {
                        viewport: Viewport::Fixed(area),
                    },
                )
                .unwrap(),
                sink,
                links: Links::default(),
                screen: Screen::default(),
                rows,
                allocation,
            }
        }

        fn window(&self) -> usize {
            usize::from(self.area.height) - 1
        }

        /// Draw `window` rows from `scroll`, selecting `selected`; returns the bytes written.
        fn draw(&mut self, scroll: usize, selected: Option<usize>) -> String {
            let shown: Vec<&MergeRequest> =
                self.rows[scroll..scroll + self.window()].iter().collect();
            let mut t = tab();
            t.select(selected.map(|i| self.rows[i].id.clone()));
            let mut spans = Vec::new();
            let completed = self
                .terminal
                .draw(|frame| {
                    let table = build(
                        &shown,
                        &t,
                        &self.allocation,
                        &theme(false),
                        now(),
                        PeopleDisplayModes::default(),
                    );
                    frame.render_widget(table, self.area);
                    spans = link_spans(
                        frame.buffer_mut(),
                        self.area,
                        &shown,
                        &self.allocation,
                        Column::Title,
                        &theme(false),
                        now(),
                    );
                })
                .unwrap();
            let buffer = completed.buffer.clone();
            self.links
                .write(self.terminal.backend_mut(), &buffer, spans)
                .unwrap();
            let written = self.sink.take();
            self.screen.feed(&written);
            written
        }

        /// Every title cell links to its own merge request, and nothing else is linked.
        fn assert_linked(&self, scroll: usize) {
            let x = column_x(&self.allocation, self.area, Column::Title).unwrap();
            let width = usize::from(self.allocation.width_of(Column::Title).unwrap());
            for offset in 0..self.window() {
                let mr = &self.rows[scroll + offset];
                let text = linked_text(mr, Column::Title, width, &theme(false), now());
                assert_row_linked(&self.screen, 1 + offset as u16, x, &text, &mr.web_url);
            }
            assert!(!self.screen.link_open());
        }
    }

    /// Titles that start and end the same and differ only in the middle, so almost every
    /// cell is identical from one frame to the next.
    fn alike(i: usize) -> String {
        format!("Fix the thing {i} in the same place")
    }

    #[test]
    fn scrolling_a_big_window_keeps_every_cell_linked_to_its_own_row() {
        let mut big = Big::new(200, alike);
        for scroll in [0, 1, 2, 5, 6, 3, 0, 100, 101, 29, 170] {
            big.draw(scroll, Some(scroll));
            big.assert_linked(scroll);
        }
    }

    /// Selecting a row changes its cell styles but not its text, which the span comparison
    /// has to notice: the row is resent by ratatui, so it has to be relinked.
    #[test]
    fn moving_the_selection_keeps_both_rows_linked() {
        let mut big = Big::new(60, alike);
        for selected in [3, 4, 5, 4, 20, 3] {
            big.draw(0, Some(selected));
            big.assert_linked(0);
        }
    }

    /// A scroll in a big window sends one open and close per row plus the text, not an
    /// escape per cell (~400 KiB before the overlay).
    #[test]
    fn a_scroll_frame_stays_small() {
        let mut big = Big::new(200, alike);
        big.draw(0, None);
        let written = big.draw(1, None);
        assert!(written.len() < 20_000, "{} bytes", written.len());
        assert_eq!(
            written.matches("\x1b]8;id=").count(),
            big.window(),
            "one link per row"
        );
    }

    /// A resize clears the screen, links included: everything has to be linked again.
    #[test]
    fn a_resize_relinks_every_row() {
        let mut big = Big::new(60, alike);
        big.draw(0, None);
        // Same rows, different buffer area: as `Terminal` hands over after a resize.
        let shown: Vec<&MergeRequest> = big.rows[..big.window()].iter().collect();
        let area = Rect {
            height: big.area.height + 1,
            ..big.area
        };
        let buffer = Buffer::empty(area);
        let spans = link_spans(
            &buffer,
            big.area,
            &shown,
            &big.allocation,
            Column::Title,
            &theme(false),
            now(),
        );
        let sink = Sink::default();
        let mut backend = CrosstermBackend::new(sink.clone());
        big.links.write(&mut backend, &buffer, spans).unwrap();
        let written = sink.take();
        assert_eq!(written.matches("\x1b]8;id=").count(), big.window());
    }

    /// A writer that fails once, after `limit` bytes, then behaves.
    struct Flaky {
        sink: Sink,
        limit: usize,
        written: usize,
    }

    impl std::io::Write for Flaky {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.written + buf.len() > self.limit && self.limit != usize::MAX {
                self.limit = usize::MAX;
                return Err(std::io::Error::other("flaky"));
            }
            self.written += buf.len();
            self.sink.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Whether `bytes` stops at the end of an escape sequence, not partway through one.
    fn ends_between_sequences(bytes: &str) -> bool {
        let Some(start) = bytes.rfind('\x1b') else {
            return true;
        };
        let rest = &bytes[start + 1..];
        match rest.chars().next() {
            None => false,
            Some('[') => rest[1..].chars().any(|c| c.is_ascii_alphabetic()),
            Some(']') => rest.ends_with("\x1b\\"),
            Some(_) => true,
        }
    }

    /// A write that dies between an open and its close must not leave the terminal inside
    /// the link, and the next frame must link everything again.
    #[test]
    fn a_failed_write_closes_the_link_and_relinks_next_time() {
        let big = Big::new(60, alike);
        let shown: Vec<&MergeRequest> = big.rows[..big.window()].iter().collect();
        let buffer = Buffer::empty(big.area);
        let spans = || {
            link_spans(
                &buffer,
                big.area,
                &shown,
                &big.allocation,
                Column::Title,
                &theme(false),
                now(),
            )
        };

        // Whichever write fails, wherever it lands in the escape/text sequence.
        for limit in (0..2_500).step_by(5) {
            let sink = Sink::default();
            let mut backend = CrosstermBackend::new(Flaky {
                sink: sink.clone(),
                limit,
                written: 0,
            });
            let mut links = Links::default();
            assert!(links.write(&mut backend, &buffer, spans()).is_err());
            let sent = sink.take();
            // A write that dies inside an escape sequence leaves the terminal in a state
            // nothing can repair; only failures between sequences are checked.
            let prefix = sent.strip_suffix(hyperlink::CLOSE).unwrap_or(&sent);
            if ends_between_sequences(prefix) {
                let mut screen = Screen::default();
                screen.feed(&sent);
                assert!(
                    !screen.link_open(),
                    "failing after {limit} bytes left the terminal inside a link"
                );
            }

            links.write(&mut backend, &buffer, spans()).unwrap();
            let written = sink.take();
            assert_eq!(
                written.matches("\x1b]8;id=").count(),
                big.window(),
                "failing after {limit} bytes"
            );
        }
    }

    /// Time and bytes of the link overlay for a one-row and a page scroll in a big window.
    #[test]
    #[ignore = "measurement: run with --release --nocapture"]
    fn measure_link_overlay_cost() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        let rows: Vec<MergeRequest> = (0..2000)
            .map(|i| {
                let mut m = mr(&i.to_string(), "jdoe");
                m.title = format!("Merge request number {i} with a reasonably long title");
                m.web_url =
                    format!("https://gitlab.example.com/group/project/-/merge_requests/{i}");
                m
            })
            .collect();
        let all_rows: Vec<&MergeRequest> = rows.iter().collect();
        let allocation = allocate(
            &Column::DEFAULT,
            area.width,
            &all_rows,
            all(PeopleDisplay::YesNo),
        );
        let window = usize::from(area.height) - 1;

        for (name, step) in [("one-row", 1usize), ("page", window)] {
            let sink = Sink::default();
            let mut terminal = Terminal::with_options(
                CrosstermBackend::new(sink.clone()),
                TerminalOptions {
                    viewport: Viewport::Fixed(area),
                },
            )
            .unwrap();
            let mut links = Links::default();
            let t = tab();
            let (mut draw_time, mut link_time, mut bytes) =
                (std::time::Duration::ZERO, std::time::Duration::ZERO, 0usize);
            let frames = 30;
            for frame_no in 0..frames {
                let scroll = frame_no * step;
                let shown = &all_rows[scroll..scroll + window];
                sink.0.lock().unwrap().clear();
                let started = std::time::Instant::now();
                let mut spans = Vec::new();
                let completed = terminal
                    .draw(|frame| {
                        let table = build(
                            shown,
                            &t,
                            &allocation,
                            &theme(false),
                            now(),
                            PeopleDisplayModes::default(),
                        );
                        frame.render_widget(table, area);
                        spans = link_spans(
                            frame.buffer_mut(),
                            area,
                            shown,
                            &allocation,
                            Column::Title,
                            &theme(false),
                            now(),
                        );
                    })
                    .unwrap();
                let buffer = completed.buffer.clone();
                draw_time += started.elapsed();
                let started = std::time::Instant::now();
                links.write(terminal.backend_mut(), &buffer, spans).unwrap();
                link_time += started.elapsed();
                bytes = sink.0.lock().unwrap().len();
            }
            println!(
                "{name}: draw+spans {:?}/frame, Links::write {:?}/frame, {bytes} bytes (last frame)",
                draw_time / frames as u32,
                link_time / frames as u32
            );
        }
    }

    /// One open and one close per title, not per cell: the URL is most of the bytes.
    #[test]
    fn each_title_is_linked_once() {
        let rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let written = Linked::new(&rows).draw(&refs, Column::Title);

        assert_eq!(written.matches("\x1b]8;id=").count(), rows.len());
        assert_eq!(written.matches(hyperlink::CLOSE).count(), rows.len());
    }

    /// Linking adds nothing to the buffer, so it cannot shift or swallow the columns after.
    #[test]
    fn the_rest_of_the_row_is_still_drawn() {
        let rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let written = Linked::new(&rows).draw(&refs, Column::Title);

        // Checked piecewise because each is written with its own colour escape between.
        for after_the_title in ["+310", "-4", "No"] {
            assert!(
                written.contains(after_the_title),
                "`{after_the_title}` was skipped after the link:\n{written:?}"
            );
        }
    }

    /// Scrolling swaps which merge request sits on a row. Cells whose glyph did not change
    /// are not resent by ratatui, yet must follow the new row's link — and cells the
    /// longer title covered must lose theirs.
    #[test]
    fn a_scrolled_redraw_relinks_every_cell_to_its_new_row() {
        let mut rows = linked_rows();
        rows[0].title = "Merge request".to_owned();
        rows[1].title = "Merge request with a longer title".to_owned();
        let forward: Vec<&MergeRequest> = rows.iter().collect();
        let backward: Vec<&MergeRequest> = rows.iter().rev().collect();
        let mut linked = Linked::new(&rows);
        let x = linked.x_of(Column::Title);

        for refs in [&forward, &backward, &forward] {
            linked.draw(refs, Column::Title);
            for (index, mr) in refs.iter().enumerate() {
                assert_row_linked(&linked.screen, 1 + index as u16, x, &mr.title, &mr.web_url);
            }
            assert!(!linked.screen.link_open());
        }
    }

    /// A live refresh that changes another column must not resend the title or its link.
    #[test]
    fn a_second_draw_leaves_an_unchanged_title_and_link_untouched() {
        let mut rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let mut linked = Linked::new(&rows);
        linked.draw(&refs, Column::Title);

        rows[0].pipeline = Some(Pipeline {
            url: "https://example.com/p".into(),
            status: PipelineStatus::Failed,
            finished_at: None,
        });
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let written = linked.draw(&refs, Column::Title);

        assert!(
            !written.contains("Merge request") && !written.contains("\x1b]8;"),
            "title was redrawn even though it did not change: {written:?}"
        );
        assert!(
            written.contains('✘'),
            "the new pipeline glyph must appear: {written:?}"
        );
    }

    /// A title of wide graphemes is linked whole and closed after its last glyph.
    #[test]
    fn a_title_of_wide_characters_is_linked_whole() {
        let mut m = mr("1", "jdoe");
        m.title = "デ".repeat(80);
        let rows = vec![m];
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let mut linked = Linked::new(&rows);
        linked.draw(&refs, Column::Title);

        let x = linked.x_of(Column::Title);
        let row = linked.screen.linked_in_row(1);
        assert!(!row.is_empty());
        assert!(
            row.iter()
                .all(|&(cx, url)| cx >= x && url == rows[0].web_url)
        );
        assert!(!linked.screen.link_open());
    }

    #[test]
    fn linking_an_empty_table_or_a_columnless_one_does_nothing() {
        let rows = linked_rows();
        let refs: Vec<&MergeRequest> = rows.iter().collect();
        let buffer = Buffer::empty(AREA);

        let allocation = allocate(
            &Column::DEFAULT,
            AREA.width,
            &refs,
            all(PeopleDisplay::YesNo),
        );
        let none = link_spans(
            &buffer,
            AREA,
            &[],
            &allocation,
            Column::Title,
            &theme(false),
            now(),
        );
        assert!(none.is_empty(), "no rows, no links");

        let columnless = allocate(
            &[Column::Author],
            AREA.width,
            &refs,
            all(PeopleDisplay::YesNo),
        );
        let none = link_spans(
            &buffer,
            AREA,
            &refs,
            &columnless,
            Column::Title,
            &theme(false),
            now(),
        );
        assert!(none.is_empty(), "no title column, no links");
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
