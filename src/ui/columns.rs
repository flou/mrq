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
//! 2. Shrinkable columns give up their slack next (author 12→8, repo 14→10). A truncated
//!    username is still identifiable; a truncated title is not more so.
//! 3. Whole columns are dropped, in a fixed order: age, then assigned, then diff.
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

/// Space between columns.
const GAP: u16 = 1;

/// The order columns are dropped in when the terminal is too narrow.
const DROP_ORDER: [Column; 3] = [Column::Age, Column::Assigned, Column::Diff];

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

const fn rules(column: Column) -> Rules {
    // `preferred == minimum` means fixed; a larger preferred means shrinkable.
    match column {
        Column::Approved => Rules {
            preferred: 4,
            minimum: 4,
            flex: false,
        },
        Column::Author => Rules {
            preferred: 12,
            minimum: 8,
            flex: false,
        },
        Column::Repo => Rules {
            preferred: 20,
            minimum: 10,
            flex: false,
        },
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
        let gaps = GAP * u16::try_from(self.widths.len().saturating_sub(1)).unwrap_or(0);
        content + gaps
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
pub fn allocate(columns: &[Column], available: u16) -> Allocation {
    let mut visible: Vec<Column> = columns.to_vec();
    let mut dropped = Vec::new();

    // Drop whole columns until the minimum layout fits. Dropping in the spec's order
    // rather than by cost keeps the result predictable across widths — a column that
    // vanishes and reappears as the window is dragged is worse than one that is simply
    // absent below a threshold.
    for candidate in DROP_ORDER {
        if minimum_width(&visible) <= available {
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
    while !visible.is_empty() && minimum_width(&visible) > available {
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

    let widths = distribute(&visible, available);
    Allocation { widths, dropped }
}

/// Width needed if every column were at its minimum.
fn minimum_width(columns: &[Column]) -> u16 {
    let content: u16 = columns.iter().map(|c| rules(*c).minimum).sum();
    let gaps = GAP * u16::try_from(columns.len().saturating_sub(1)).unwrap_or(0);
    content.saturating_add(gaps)
}

fn distribute(columns: &[Column], available: u16) -> Vec<(Column, u16)> {
    let gaps = GAP * u16::try_from(columns.len().saturating_sub(1)).unwrap_or(0);
    let for_content = available.saturating_sub(gaps);

    // Start at minimums — guaranteed to fit, since the caller has already dropped
    // columns until it does — then hand out what is left.
    let mut widths: Vec<(Column, u16)> = columns.iter().map(|c| (*c, rules(*c).minimum)).collect();
    let mut spare = for_content.saturating_sub(widths.iter().map(|(_, w)| *w).sum::<u16>());

    // Non-flex columns reach their preferred width first, so the table looks the same at
    // 200 columns as at 120 apart from the title.
    for (column, width) in &mut widths {
        if spare == 0 {
            break;
        }
        let rules = rules(*column);
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
    if let Some((_, width)) = widths.iter_mut().find(|(c, _)| rules(*c).flex) {
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
            let allocation = allocate(&default_columns(), width);
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
                let allocation = allocate(&subset, width);
                assert!(allocation.total() <= width, "{take} cols at {width}");
            }
        }
    }

    /// The column set and its order come from configuration.
    #[test]
    fn the_configured_order_is_preserved() {
        let columns = vec![Column::Title, Column::Author, Column::Updated];
        let allocation = allocate(&columns, 120);

        assert_eq!(visible(&allocation), columns);
    }

    #[test]
    fn a_wide_terminal_gives_every_column_its_preferred_width() {
        let allocation = allocate(&default_columns(), 200);

        assert!(allocation.dropped.is_empty());
        assert_eq!(allocation.width_of(Column::Author), Some(12));
        assert_eq!(allocation.width_of(Column::Repo), Some(20));
        assert_eq!(allocation.width_of(Column::Approved), Some(4));
        assert_eq!(allocation.width_of(Column::Pipeline), Some(2));
        assert_eq!(allocation.width_of(Column::Diff), Some(11));
    }

    /// The title is the one column useful at any width, so it takes the slack.
    #[test]
    fn the_title_absorbs_all_remaining_space() {
        let narrow = allocate(&default_columns(), 120);
        let wide = allocate(&default_columns(), 200);

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
            let allocation = allocate(&default_columns(), width);
            if let Some(title) = allocation.width_of(Column::Title) {
                assert!(title >= 20, "title {title} at width {width}");
            }
        }
    }

    /// Shrinkable columns give up width before the title does.
    #[test]
    fn shrinkable_columns_give_up_slack_before_the_title() {
        // Just enough that not everything can be preferred.
        let allocation = allocate(&[Column::Author, Column::Repo, Column::Title], 46);

        assert_eq!(allocation.width_of(Column::Title), Some(20), "at minimum");
        assert!(allocation.width_of(Column::Author).unwrap() >= 8);
        assert!(allocation.width_of(Column::Repo).unwrap() >= 10);
    }

    #[test]
    fn shrinkable_columns_respect_their_floors() {
        for width in 0..=300u16 {
            let allocation = allocate(&default_columns(), width);
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
            let now = visible(&allocate(&columns, width));
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
        let wide = allocate(&default_columns(), 200);
        assert_eq!(wide.dropped_note(), None);

        let narrow = allocate(&default_columns(), 70);
        let note = narrow.dropped_note().expect("something was dropped");
        assert!(note.contains("age"), "{note}");
        assert!(!narrow.dropped.is_empty());
    }

    /// The widths the spec calls out, at the sizes the issue names.
    #[test]
    fn the_documented_widths_hold_at_each_tested_size() {
        for width in [60, 80, 100, 120, 200] {
            let allocation = allocate(&default_columns(), width);

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
            let allocation = allocate(&default_columns(), width);
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
        let allocation = allocate(&default_columns(), 40);

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
        let allocation = allocate(&default_columns(), 0);

        assert!(allocation.is_empty());
        assert_eq!(allocation.total(), 0);
        assert_eq!(allocation.dropped.len(), Column::DEFAULT.len());
    }

    #[test]
    fn an_empty_column_set_allocates_nothing() {
        let allocation = allocate(&[], 200);
        assert!(allocation.is_empty());
        assert!(allocation.dropped.is_empty());
    }

    /// With no flex column the table is narrower than the pane, which is correct —
    /// stretching fixed columns would only scatter the content.
    #[test]
    fn a_column_set_without_a_title_does_not_stretch() {
        let columns = vec![Column::Author, Column::Repo, Column::Updated];
        let allocation = allocate(&columns, 200);

        assert_eq!(allocation.width_of(Column::Author), Some(12));
        assert_eq!(allocation.width_of(Column::Repo), Some(20));
        assert!(allocation.total() < 200);
    }

    /// Gaps are part of the budget; forgetting them is how a table ends up one column
    /// too wide on exactly the widths where it matters.
    #[test]
    fn gaps_between_columns_are_counted() {
        let columns = vec![Column::Pipeline, Column::Assigned];
        let allocation = allocate(&columns, 200);

        assert_eq!(allocation.total(), 2 + GAP + 4);
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
            assert_eq!(allocate(&columns, width), allocate(&columns, width));
        }
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
            let allocation = allocate(&Column::DEFAULT, width);
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
