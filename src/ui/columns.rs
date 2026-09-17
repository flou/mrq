//! Column widths: fitting the table into whatever width there is.
//!
//! The only genuinely fiddly piece of layout in the program, so it is a pure function of
//! `(columns, width)` and tested away from rendering.
//!
//! # The order things give way
//!
//! Width is taken back in the order that costs the user least:
//!
//! 1. `title` absorbs slack, down to its minimum — it is the one column that is useful
//!    at any width, and truncating a title still leaves it recognisable.
//! 2. Shrinkable columns give up their slack next (author and repo both fit their
//!    content, down to 8 and 10 respectively). A truncated username or repo name is
//!    still identifiable; a truncated title is not more so.
//! 3. Whole columns are dropped, in a fixed order: age, then assigned, then diff.
//!
//! # Author and repo size to their content
//!
//! Unlike the other columns, `author` and `repo`'s *preferred* widths are not constants:
//! the caller measures the widest author name and project name on the rows currently in
//! the tab and passes them in as [`Fitted`]. This keeps [`allocate`] itself a pure
//! function of `(columns, available, fitted)` — it does not reach into row data on its
//! own — while letting each column show its content in full on an instance where it is
//! short, and give the difference back to `title`, rather than wasting the fixed width or
//! truncating every row on an instance where it is long.
//!
//! Dropping is last because a missing column is invisible — the user cannot tell a
//! dropped column from one that never existed, which is why [`Allocation`] reports what
//! it removed so the status bar can say so.

use crate::config::schema::Column;

/// One column's width rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rules {
    /// Width when there is room for everything.
    preferred: u16,
    /// Smallest width that still shows something useful.
    minimum: u16,
    /// Takes all remaining space. Only `title` does.
    flex: bool,
}

/// Space between columns, the default.
const GAP: u16 = 1;

/// Space either side of `repo`: it sits between `author` and `title`, the two other
/// columns whose width follows their content rather than a fixed rule, and a single-cell
/// gap reads as part of whichever neighbour's text is longest that frame rather than as a
/// boundary. Wherever `repo` is adjacent to `author` or `title`, in either order, this
/// replaces the default gap.
const WIDE_GAP: u16 = 2;

/// The gap between two columns that sit next to each other in display order.
///
/// `pub(crate)` rather than private: `table` has to draw exactly the gaps this module
/// budgeted for, or a wide `repo` boundary would be sized here and drawn elsewhere as the
/// default single cell.
pub(crate) const fn gap_between(left: Column, right: Column) -> u16 {
    use Column::{Author, Repo, Title};
    match (left, right) {
        (Repo, Author) | (Author, Repo) | (Repo, Title) | (Title, Repo) => WIDE_GAP,
        _ => GAP,
    }
}

/// The total width `columns` spends on the gaps between them, in display order.
fn total_gaps(columns: &[Column]) -> u16 {
    columns
        .windows(2)
        .map(|pair| gap_between(pair[0], pair[1]))
        .sum()
}

/// Upper bound on either content-fitted column (`author`, `repo`).
pub const FIT_MAX: u16 = 30;

/// The order columns are dropped in when the terminal is too narrow.
const DROP_ORDER: [Column; 3] = [Column::Age, Column::Assigned, Column::Diff];

/// Widths measured from the rows on screen, for columns that size to their content.
///
/// `None` means "no measurement" — the column falls back to its fixed `preferred`. Kept
/// apart from [`Rules`] so `allocate` stays a pure function of its arguments rather than
/// reaching for row data itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Fitted {
    /// Widest author name on the current rows, already clamped by the caller to
    /// `header().width() ..= FIT_MAX`.
    pub author: Option<u16>,
    /// Widest project name on the current rows, already clamped by the caller to
    /// `header().width() ..= FIT_MAX`.
    pub repo: Option<u16>,
}

/// How much a column is worth keeping, for widths below what whole-column dropping covers.
///
/// Lower is dropped sooner. Display order is the wrong thing to fall back on: it would
/// drop `title` — the only column that identifies which merge request a row is — while
/// keeping `approved`, because `title` sits further right in the default set.
const fn keep_priority(column: Column) -> u8 {
    match column {
        Column::Title => 9,
        Column::Repo => 8,
        Column::Author => 7,
        Column::Approved => 6,
        Column::Pipeline => 5,
        Column::Updated => 4,
        Column::Age => 3,
        Column::Assigned => 2,
        Column::Diff => 1,
        Column::Branch => 6,
    }
}

const fn rules(column: Column, fitted: Fitted) -> Rules {
    // `preferred == minimum` means fixed; a larger preferred means shrinkable.
    match column {
        // Wide enough for the mark plus one cell of padding; there is no header text to
        // fit any more.
        Column::Approved => Rules {
            preferred: 2,
            minimum: 2,
            flex: false,
        },
        // `preferred` comes from the caller's measurement, falling back to the old fixed
        // 12 when there is nothing to measure (an empty tab). `minimum` stays the usual
        // shrinkable floor of 8 — unless the fitted width is already narrower, in which
        // case there is no slack to give up and the column is effectively fixed.
        Column::Author => {
            let preferred = match fitted.author {
                Some(width) => width,
                None => 12,
            };
            Rules {
                preferred,
                minimum: if preferred < 8 { preferred } else { 8 },
                flex: false,
            }
        }
        // Same treatment as `author`: preferred comes from the measurement, falling back
        // to the old fixed 20 with nothing to measure; minimum stays the usual shrinkable
        // floor of 10, unless the fitted width is already narrower.
        Column::Repo => {
            let preferred = match fitted.repo {
                Some(width) => width,
                None => 20,
            };
            Rules {
                preferred,
                minimum: if preferred < 10 { preferred } else { 10 },
                flex: false,
            }
        }
        Column::Title => Rules {
            preferred: 40,
            minimum: 20,
            flex: true,
        },
        Column::Pipeline => Rules {
            preferred: 2,
            minimum: 2,
            flex: false,
        },
        Column::Assigned => Rules {
            preferred: 4,
            minimum: 4,
            flex: false,
        },
        Column::Age => Rules {
            preferred: 6,
            minimum: 6,
            flex: false,
        },
        Column::Updated => Rules {
            preferred: 7,
            minimum: 7,
            flex: false,
        },
        Column::Diff => Rules {
            preferred: 11,
            minimum: 11,
            flex: false,
        },
        Column::Branch => Rules {
            preferred: 24,
            minimum: 12,
            flex: false,
        },
    }
}

/// What fitted, and what did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    /// Visible columns in display order, with the width each gets.
    pub widths: Vec<(Column, u16)>,
    /// Columns dropped for lack of room, in the order they were dropped.
    pub dropped: Vec<Column>,
}

impl Allocation {
    /// Total width used, including the gaps between columns.
    #[cfg(test)]
    fn total(&self) -> u16 {
        let content: u16 = self.widths.iter().map(|(_, w)| *w).sum();
        let columns: Vec<Column> = self.widths.iter().map(|(c, _)| *c).collect();
        content + total_gaps(&columns)
    }

    pub fn width_of(&self, column: Column) -> Option<u16> {
        self.widths
            .iter()
            .find(|(c, _)| *c == column)
            .map(|(_, w)| *w)
    }

    #[cfg(test)]
    const fn is_empty(&self) -> bool {
        self.widths.is_empty()
    }

    /// A note for the status bar, when columns had to be dropped. Not currently wired to
    /// the status bar: `statusbar::Status` carries its own `dropped` field, set directly
    /// from the same `Allocation`, and formats its own line instead of calling this.
    #[cfg(test)]
    fn dropped_note(&self) -> Option<String> {
        if self.dropped.is_empty() {
            return None;
        }
        let names: Vec<&str> = self.dropped.iter().map(|c| c.key()).collect();
        Some(format!("narrow: hiding {}", names.join(", ")))
    }
}

/// Columns held back until wide mode is on: `diff`, plus whatever `wide_columns` names.
/// Order and the rest of the set are untouched — only wide-only columns are removed.
pub fn for_wide_mode(columns: &[Column], wide_columns: &[Column], wide: bool) -> Vec<Column> {
    if wide {
        return columns.to_vec();
    }
    columns
        .iter()
        .copied()
        .filter(|c| *c != Column::Diff && !wide_columns.contains(c))
        .collect()
}

/// Allocate widths for `columns` within `available`.
///
/// `fitted` carries widths measured from the current rows, for columns whose preferred
/// width is content-dependent rather than a fixed constant (currently just `author`); pass
/// [`Fitted::default`] to get the old fixed-width behaviour for every column.
pub fn allocate(columns: &[Column], available: u16, fitted: Fitted) -> Allocation {
    let mut visible: Vec<Column> = columns.to_vec();
    let mut dropped = Vec::new();

    // Drop whole columns until the minimum layout fits. Dropping in the spec's order
    // rather than by cost keeps the result predictable across widths — a column that
    // vanishes and reappears as the window is dragged is worse than one that is simply
    // absent below a threshold.
    for candidate in DROP_ORDER {
        if minimum_width(&visible, fitted) <= available {
            break;
        }
        if let Some(index) = visible.iter().position(|c| *c == candidate) {
            visible.remove(index);
            dropped.push(candidate);
        }
    }

    // Still too narrow even at minimums: keep dropping, least valuable first, so a very
    // small pane shows the columns that identify a row rather than whichever happen to
    // be leftmost.
    while !visible.is_empty() && minimum_width(&visible, fitted) > available {
        let Some(index) = visible
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| keep_priority(**c))
            .map(|(i, _)| i)
        else {
            break;
        };
        dropped.push(visible.remove(index));
    }

    if visible.is_empty() {
        return Allocation {
            widths: Vec::new(),
            dropped,
        };
    }

    let widths = distribute(&visible, available, fitted);
    Allocation { widths, dropped }
}

/// Width needed if every column were at its minimum.
fn minimum_width(columns: &[Column], fitted: Fitted) -> u16 {
    let content: u16 = columns.iter().map(|c| rules(*c, fitted).minimum).sum();
    content.saturating_add(total_gaps(columns))
}

fn distribute(columns: &[Column], available: u16, fitted: Fitted) -> Vec<(Column, u16)> {
    let gaps = total_gaps(columns);
    let for_content = available.saturating_sub(gaps);

    // Start at minimums — guaranteed to fit, since the caller has already dropped
    // columns until it does — then hand out what is left.
    let mut widths: Vec<(Column, u16)> = columns
        .iter()
        .map(|c| (*c, rules(*c, fitted).minimum))
        .collect();
    let mut spare = for_content.saturating_sub(widths.iter().map(|(_, w)| *w).sum::<u16>());

    // Non-flex columns reach their preferred width first, so the table looks the same at
    // 200 columns as at 120 apart from the title.
    for (column, width) in &mut widths {
        if spare == 0 {
            break;
        }
        let rules = rules(*column, fitted);
        if rules.flex {
            continue;
        }
        let want = rules.preferred.saturating_sub(*width);
        let give = want.min(spare);
        *width += give;
        spare -= give;
    }

    // Everything left goes to the flex column. With no flex column the table is simply
    // narrower than the pane, which is correct — stretching fixed columns would only
    // scatter the content.
    if let Some((_, width)) = widths.iter_mut().find(|(c, _)| rules(*c, fitted).flex) {
        *width += spare;
    }

    widths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_columns() -> Vec<Column> {
        Column::DEFAULT.to_vec()
    }

    fn visible(allocation: &Allocation) -> Vec<Column> {
        allocation.widths.iter().map(|(c, _)| *c).collect()
    }

    /// The invariant that matters: an over-wide table corrupts every row.
    #[test]
    fn allocation_never_exceeds_the_available_width() {
        for width in 0..=300u16 {
            let allocation = allocate(&default_columns(), width, Fitted::default());
            assert!(
                allocation.total() <= width,
                "width {width}: allocated {} for {:?}",
                allocation.total(),
                visible(&allocation)
            );
        }
    }

    #[test]
    fn allocation_is_within_bounds_for_every_column_subset() {
        let columns = default_columns();
        for take in 1..=columns.len() {
            let subset: Vec<Column> = columns.iter().copied().take(take).collect();
            for width in [0, 1, 20, 40, 60, 79, 80, 100, 120, 200, 400] {
                let allocation = allocate(&subset, width, Fitted::default());
                assert!(allocation.total() <= width, "{take} cols at {width}");
            }
        }
    }

    /// The column set and its order come from configuration.
    #[test]
    fn the_configured_order_is_preserved() {
        let columns = vec![Column::Title, Column::Author, Column::Updated];
        let allocation = allocate(&columns, 120, Fitted::default());

        assert_eq!(visible(&allocation), columns);
    }

    #[test]
    fn a_wide_terminal_gives_every_column_its_preferred_width() {
        let allocation = allocate(&default_columns(), 200, Fitted::default());

        assert!(allocation.dropped.is_empty());
        assert_eq!(allocation.width_of(Column::Author), Some(12));
        assert_eq!(allocation.width_of(Column::Repo), Some(20));
        assert_eq!(allocation.width_of(Column::Approved), Some(2));
        assert_eq!(allocation.width_of(Column::Pipeline), Some(2));
        assert_eq!(allocation.width_of(Column::Diff), Some(11));
    }

    /// The title is the one column useful at any width, so it takes the slack.
    #[test]
    fn the_title_absorbs_all_remaining_space() {
        let narrow = allocate(&default_columns(), 120, Fitted::default());
        let wide = allocate(&default_columns(), 200, Fitted::default());

        let narrow_title = narrow.width_of(Column::Title).unwrap();
        let wide_title = wide.width_of(Column::Title).unwrap();

        assert_eq!(
            wide_title - narrow_title,
            80,
            "the extra 80 columns all went to the title"
        );
        for column in [Column::Author, Column::Repo, Column::Diff] {
            assert_eq!(
                narrow.width_of(column),
                wide.width_of(column),
                "{column:?} should not change with terminal width"
            );
        }
    }

    #[test]
    fn the_title_never_falls_below_its_minimum() {
        for width in 0..=300u16 {
            let allocation = allocate(&default_columns(), width, Fitted::default());
            if let Some(title) = allocation.width_of(Column::Title) {
                assert!(title >= 20, "title {title} at width {width}");
            }
        }
    }

    /// Shrinkable columns give up width before the title does.
    #[test]
    fn shrinkable_columns_give_up_slack_before_the_title() {
        // Just enough that not everything can be preferred.
        let allocation = allocate(
            &[Column::Author, Column::Repo, Column::Title],
            46,
            Fitted::default(),
        );

        assert_eq!(allocation.width_of(Column::Title), Some(20), "at minimum");
        assert!(allocation.width_of(Column::Author).unwrap() >= 8);
        assert!(allocation.width_of(Column::Repo).unwrap() >= 10);
    }

    #[test]
    fn shrinkable_columns_respect_their_floors() {
        for width in 0..=300u16 {
            let allocation = allocate(&default_columns(), width, Fitted::default());
            if let Some(author) = allocation.width_of(Column::Author) {
                assert!(author >= 8, "author {author} at {width}");
            }
            if let Some(repo) = allocation.width_of(Column::Repo) {
                assert!(repo >= 10, "repo {repo} at {width}");
            }
        }
    }

    /// The drop order when columns don't fit: age, then assigned, then diff.
    #[test]
    fn columns_are_dropped_in_the_documented_order() {
        let columns = default_columns();

        // Walk widths downward and record the order columns disappear.
        let mut order = Vec::new();
        let mut previous: Vec<Column> = columns.clone();
        for width in (40..=120u16).rev() {
            let now = visible(&allocate(&columns, width, Fitted::default()));
            for column in &previous {
                if !now.contains(column) && !order.contains(column) {
                    order.push(*column);
                }
            }
            previous = now;
        }

        let first_three: Vec<Column> = order.iter().copied().take(3).collect();
        assert_eq!(
            first_three,
            [Column::Age, Column::Assigned, Column::Diff],
            "full drop order observed: {order:?}"
        );
    }

    /// A missing column is invisible — the user cannot tell it from one that never
    /// existed — so the omission has to be reported.
    #[test]
    fn dropped_columns_are_reported_for_the_status_bar() {
        let wide = allocate(&default_columns(), 200, Fitted::default());
        assert_eq!(wide.dropped_note(), None);

        let narrow = allocate(&default_columns(), 70, Fitted::default());
        let note = narrow.dropped_note().expect("something was dropped");
        assert!(note.contains("age"), "{note}");
        assert!(!narrow.dropped.is_empty());
    }

    /// The widths the spec calls out, at the sizes the issue names.
    #[test]
    fn the_documented_widths_hold_at_each_tested_size() {
        for width in [60, 80, 100, 120, 200] {
            let allocation = allocate(&default_columns(), width, Fitted::default());

            assert!(allocation.total() <= width, "at {width}");
            assert!(
                allocation.width_of(Column::Title).is_some(),
                "the title survives at {width}"
            );
            if width >= 100 {
                assert!(
                    allocation.dropped.is_empty(),
                    "nothing should drop at {width}: {:?}",
                    allocation.dropped
                );
            }
        }
    }

    /// The title identifies which merge request a row is, so it is the last thing to go.
    /// Falling back to display order would drop it while keeping `approved`.
    #[test]
    fn the_title_is_the_last_column_standing() {
        for width in 1..=90u16 {
            let allocation = allocate(&default_columns(), width, Fitted::default());
            if allocation.is_empty() {
                continue;
            }
            assert!(
                allocation.width_of(Column::Title).is_some(),
                "title dropped at width {width}, leaving {:?}",
                visible(&allocation)
            );
        }
    }

    /// Below the widths whole-column dropping covers, columns go least-valuable first.
    #[test]
    fn very_narrow_widths_drop_by_value_not_by_position() {
        let allocation = allocate(&default_columns(), 40, Fitted::default());

        assert!(allocation.total() <= 40);
        let kept = visible(&allocation);
        assert!(kept.contains(&Column::Title), "{kept:?}");
        assert!(
            !kept.contains(&Column::Diff) && !kept.contains(&Column::Assigned),
            "cheap columns go first: {kept:?}"
        );
    }

    #[test]
    fn a_zero_width_allocates_nothing() {
        let allocation = allocate(&default_columns(), 0, Fitted::default());

        assert!(allocation.is_empty());
        assert_eq!(allocation.total(), 0);
        assert_eq!(allocation.dropped.len(), Column::DEFAULT.len());
    }

    #[test]
    fn an_empty_column_set_allocates_nothing() {
        let allocation = allocate(&[], 200, Fitted::default());
        assert!(allocation.is_empty());
        assert!(allocation.dropped.is_empty());
    }

    /// With no flex column the table is narrower than the pane, which is correct —
    /// stretching fixed columns would only scatter the content.
    #[test]
    fn a_column_set_without_a_title_does_not_stretch() {
        let columns = vec![Column::Author, Column::Repo, Column::Updated];
        let allocation = allocate(&columns, 200, Fitted::default());

        assert_eq!(allocation.width_of(Column::Author), Some(12));
        assert_eq!(allocation.width_of(Column::Repo), Some(20));
        assert!(allocation.total() < 200);
    }

    /// Gaps are part of the budget; forgetting them is how a table ends up one column
    /// too wide on exactly the widths where it matters.
    #[test]
    fn gaps_between_columns_are_counted() {
        let columns = vec![Column::Pipeline, Column::Assigned];
        let allocation = allocate(&columns, 200, Fitted::default());

        assert_eq!(allocation.total(), 2 + GAP + 4);
    }

    /// AUTHOR and REPO, and REPO and TITLE, get the wider gap; every other boundary keeps
    /// the default single cell.
    #[test]
    fn repo_gets_the_wide_gap_on_both_sides() {
        assert_eq!(gap_between(Column::Author, Column::Repo), WIDE_GAP);
        assert_eq!(gap_between(Column::Repo, Column::Author), WIDE_GAP);
        assert_eq!(gap_between(Column::Repo, Column::Title), WIDE_GAP);
        assert_eq!(gap_between(Column::Title, Column::Repo), WIDE_GAP);

        assert_eq!(gap_between(Column::Approved, Column::Author), GAP);
        assert_eq!(gap_between(Column::Title, Column::Pipeline), GAP);
        assert_eq!(
            gap_between(Column::Author, Column::Title),
            GAP,
            "no repo between them"
        );
    }

    /// Author, repo and title stay adjacent to each other in the default layout — nothing
    /// gets dropped or reordered between them — so the boundaries the wide gap applies to
    /// actually occur, at every width where all three are still visible.
    #[test]
    fn author_and_repo_stay_adjacent_to_their_wide_neighbour_at_every_width() {
        for width in [80u16, 100, 120, 200] {
            let allocation = allocate(&default_columns(), width, Fitted::default());
            let order: Vec<Column> = visible(&allocation);

            let author_index = order.iter().position(|c| *c == Column::Author);
            let repo_index = order.iter().position(|c| *c == Column::Repo);
            let title_index = order.iter().position(|c| *c == Column::Title);

            if let (Some(a), Some(r)) = (author_index, repo_index) {
                assert_eq!(r, a + 1, "author and repo are not adjacent at {width}");
            }
            if let (Some(r), Some(t)) = (repo_index, title_index) {
                assert_eq!(t, r + 1, "repo and title are not adjacent at {width}");
            }
        }
    }

    #[test]
    fn diff_is_hidden_outside_wide_mode_even_with_no_configured_wide_columns() {
        let columns = default_columns();
        assert!(!for_wide_mode(&columns, &[], false).contains(&Column::Diff));
        assert!(for_wide_mode(&columns, &[], true).contains(&Column::Diff));
    }

    #[test]
    fn configured_wide_columns_are_hidden_only_outside_wide_mode() {
        let columns = default_columns();
        let wide_columns = [Column::Age, Column::Assigned];

        let narrow = for_wide_mode(&columns, &wide_columns, false);
        assert!(!narrow.contains(&Column::Age));
        assert!(!narrow.contains(&Column::Assigned));
        assert!(narrow.contains(&Column::Title), "other columns stay");

        let wide = for_wide_mode(&columns, &wide_columns, true);
        assert_eq!(wide, columns, "wide mode restores every configured column");
    }

    #[test]
    fn wide_mode_filtering_preserves_order() {
        let columns = default_columns();
        let narrow = for_wide_mode(&columns, &[Column::Age], false);

        assert_eq!(
            narrow,
            columns
                .into_iter()
                .filter(|c| *c != Column::Age && *c != Column::Diff)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn allocation_is_deterministic() {
        let columns = default_columns();
        for width in [60, 80, 100, 120, 200] {
            assert_eq!(
                allocate(&columns, width, Fitted::default()),
                allocate(&columns, width, Fitted::default())
            );
        }
    }

    /// A fitted width becomes `author`'s preferred width, so a wide terminal shows the
    /// whole content-fitted column rather than the old fixed 12.
    #[test]
    fn a_fitted_author_width_becomes_the_preferred_width() {
        let fitted = Fitted {
            author: Some(20),
            ..Fitted::default()
        };
        let allocation = allocate(&default_columns(), 200, fitted);

        assert_eq!(allocation.width_of(Column::Author), Some(20));
    }

    /// No measurement (an empty tab) falls back to the old fixed width.
    #[test]
    fn no_measurement_falls_back_to_the_fixed_author_width() {
        let allocation = allocate(&default_columns(), 200, Fitted::default());
        assert_eq!(allocation.width_of(Column::Author), Some(12));
    }

    /// A fitted width still gives up slack on a narrow terminal, down to the usual floor
    /// of 8 — sizing to content does not exempt the column from shrinking.
    #[test]
    fn a_fitted_author_still_shrinks_on_a_narrow_terminal() {
        let fitted = Fitted {
            author: Some(20),
            ..Fitted::default()
        };
        let columns = vec![Column::Author, Column::Repo, Column::Title];

        // Exactly the sum of every column's minimum plus its gaps: no spare to hand out.
        let narrow = allocate(&columns, 42, fitted);
        assert_eq!(
            narrow.width_of(Column::Author),
            Some(8),
            "shrunk to the floor"
        );

        let wide = allocate(&columns, 200, fitted);
        assert_eq!(
            wide.width_of(Column::Author),
            Some(20),
            "fitted width honoured"
        );
    }

    /// A fitted width below the usual floor of 8 must not be inflated back up to it — the
    /// column should not claim more room than its content needs.
    #[test]
    fn a_fitted_width_below_the_usual_floor_is_not_inflated() {
        let fitted = Fitted {
            author: Some(6),
            ..Fitted::default()
        };
        let allocation = allocate(&default_columns(), 200, fitted);

        assert_eq!(allocation.width_of(Column::Author), Some(6));
    }

    /// The invariant that matters, swept across every fitted author and repo width the
    /// caller can pass in, together — an over-wide table corrupts every row regardless of
    /// what triggered it.
    #[test]
    fn allocation_never_exceeds_available_width_for_any_fitted_width() {
        for author in [None, Some(6), Some(20), Some(FIT_MAX)] {
            for repo in [None, Some(10), Some(20), Some(FIT_MAX)] {
                let fitted = Fitted { author, repo };
                for width in [0, 20, 40, 60, 80, 100, 120, 200, 300] {
                    let allocation = allocate(&default_columns(), width, fitted);
                    assert!(
                        allocation.total() <= width,
                        "author {author:?}, repo {repo:?}, width {width}: allocated {}",
                        allocation.total()
                    );
                }
            }
        }
    }

    /// `repo` mirrors `author`: a fitted width becomes its preferred width.
    #[test]
    fn a_fitted_repo_width_becomes_the_preferred_width() {
        let fitted = Fitted {
            repo: Some(15),
            ..Fitted::default()
        };
        let allocation = allocate(&default_columns(), 200, fitted);

        assert_eq!(allocation.width_of(Column::Repo), Some(15));
    }

    /// No measurement (an empty tab) falls back to the old fixed width.
    #[test]
    fn no_measurement_falls_back_to_the_fixed_repo_width() {
        let allocation = allocate(&default_columns(), 200, Fitted::default());
        assert_eq!(allocation.width_of(Column::Repo), Some(20));
    }

    /// A fitted repo width still gives up slack on a narrow terminal, down to the usual
    /// floor of 10.
    #[test]
    fn a_fitted_repo_still_shrinks_on_a_narrow_terminal() {
        let fitted = Fitted {
            repo: Some(28),
            ..Fitted::default()
        };
        let columns = vec![Column::Author, Column::Repo, Column::Title];

        // Exactly the sum of every column's minimum plus its gaps: no spare to hand out.
        let narrow = allocate(&columns, 42, fitted);
        assert_eq!(
            narrow.width_of(Column::Repo),
            Some(10),
            "shrunk to the floor"
        );

        let wide = allocate(&columns, 200, fitted);
        assert_eq!(
            wide.width_of(Column::Repo),
            Some(28),
            "fitted width honoured"
        );
    }

    /// A fitted repo width below the usual floor of 10 must not be inflated back up to it.
    #[test]
    fn a_fitted_repo_width_below_the_usual_floor_is_not_inflated() {
        let fitted = Fitted {
            repo: Some(4),
            ..Fitted::default()
        };
        let allocation = allocate(&default_columns(), 200, fitted);

        assert_eq!(allocation.width_of(Column::Repo), Some(4));
    }

    /// Author and repo are fitted independently — sizing one to its content does not
    /// disturb the other.
    #[test]
    fn author_and_repo_are_fitted_independently() {
        let fitted = Fitted {
            author: Some(9),
            repo: Some(25),
        };
        let allocation = allocate(&default_columns(), 200, fitted);

        assert_eq!(allocation.width_of(Column::Author), Some(9));
        assert_eq!(allocation.width_of(Column::Repo), Some(25));
    }
}

#[cfg(test)]
mod preview {
    use super::*;

    /// `cargo test -- --ignored --nocapture preview_allocations`
    #[test]
    #[ignore = "reports allocations for eyeballing; does not assert"]
    fn preview_allocations() {
        for width in [60, 80, 100, 120, 200] {
            let allocation = allocate(&Column::DEFAULT, width, Fitted::default());
            let cells: Vec<String> = allocation
                .widths
                .iter()
                .map(|(c, w)| format!("{}:{w}", c.key()))
                .collect();
            println!(
                "{width:>3} -> total {:>3} | {} | dropped: {:?}",
                allocation.total(),
                cells.join(" "),
                allocation.dropped
            );
        }
    }
}
