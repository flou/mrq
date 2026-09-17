//! Frame regions: where the tab bar, table panel and status bar go.
//!
//! A pure function from terminal size to a set of rectangles, so every
//! question about "does this fit" is answered in one place and can be tested without a
//! terminal.
//!
//! # Degenerate sizes
//!
//! Terminals get resized to absurd shapes — a dragged window edge passes through every
//! width on the way, and tmux panes can be two rows tall. Every branch here yields
//! rectangles that fit inside the area given, including when that area is empty. A
//! layout that returns an out-of-bounds `Rect` panics inside ratatui at draw time, which
//! on the alternate screen means a corrupted terminal rather than a clean error.

use ratatui::layout::Rect;

/// Under these the panel border is dropped. A frame drawn around one row of table is all
/// frame and no table, and the rows are what the user came for.
const MIN_HEIGHT_FOR_BORDER: u16 = 5;
const MIN_WIDTH_FOR_BORDER: u16 = 20;

/// Where each region goes for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub tab_bar: Rect,
    /// The table panel, border included.
    pub table: Rect,
    pub status_bar: Rect,
}

impl Frame {
    /// The area popups are centred over.
    pub const fn body(&self) -> Rect {
        self.table
    }

    /// Whether the panel is drawn with a border at this size.
    pub const fn bordered(&self) -> bool {
        self.table.height >= MIN_HEIGHT_FOR_BORDER && self.table.width >= MIN_WIDTH_FOR_BORDER
    }

    /// Where the table itself is drawn: inside the border, when there is one.
    pub const fn table_inner(&self) -> Rect {
        if !self.bordered() {
            return self.table;
        }
        Rect {
            x: self.table.x + 1,
            y: self.table.y + 1,
            width: self.table.width - 2,
            height: self.table.height - 2,
        }
    }
}

/// Split a terminal area into its three regions: tab bar, table panel, status bar.
pub const fn compute(area: Rect) -> Frame {
    if area.width == 0 || area.height == 0 {
        let empty = Rect {
            x: area.x,
            y: area.y,
            width: 0,
            height: 0,
        };
        return Frame {
            tab_bar: empty,
            table: empty,
            status_bar: empty,
        };
    }

    // Vertical: one line each for the tab bar and status bar, everything else to the
    // body. On a very short terminal the chrome is given up before the content is — a
    // one-row table is still useful, a one-row status bar with no table is not.
    let (tab_height, status_height) = match area.height {
        0 | 1 => (0, 0),
        2 => (1, 0),
        _ => (1, 1),
    };
    let body_height = area.height - tab_height - status_height;

    Frame {
        tab_bar: gutters(Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: tab_height,
        }),
        table: Rect {
            x: area.x,
            y: area.y + tab_height,
            width: area.width,
            height: body_height,
        },
        status_bar: gutters(Rect {
            x: area.x,
            y: area.y + tab_height + body_height,
            width: area.width,
            height: status_height,
        }),
    }
}

/// A cell of breathing room on each side, so the bars line up with the panel's contents
/// instead of hugging the edge of the terminal.
const fn gutters(rect: Rect) -> Rect {
    if rect.width < 4 {
        return rect;
    }
    Rect {
        x: rect.x + 1,
        width: rect.width - 2,
        ..rect
    }
}

/// A rectangle centred over `area`, sized as a percentage of it.
///
/// Clamped to the area rather than allowed to overflow: a popup larger than the screen
/// is a panic inside ratatui, and the sizes here are percentages of a terminal whose
/// size nobody controls.
pub fn centred(area: Rect, width_pct: u8, height_pct: u8) -> Rect {
    let width = (u32::from(area.width) * u32::from(width_pct.min(100)) / 100) as u16;
    let height = (u32::from(area.height) * u32::from(height_pct.min(100)) / 100) as u16;

    let width = width.clamp(0, area.width);
    let height = height.clamp(0, area.height);

    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// A centred rectangle with a minimum size, shrinking to the area when it cannot fit.
#[cfg(test)]
fn centred_at_least(area: Rect, min_width: u16, min_height: u16, pct: u8) -> Rect {
    let wanted = centred(area, pct, pct);
    let width = wanted.width.max(min_width).min(area.width);
    let height = wanted.height.max(min_height).min(area.height);

    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.width <= outer.x + outer.width
            && inner.y + inner.height <= outer.y + outer.height
    }

    /// Tab bar, table panel, status bar, top to bottom.
    #[test]
    fn the_three_regions_stack_in_order() {
        let frame = compute(area(120, 30));

        assert_eq!(frame.tab_bar.y, 0);
        assert_eq!(frame.tab_bar.height, 1);
        assert_eq!(frame.tab_bar.x, 1, "a cell of gutter on each side");
        assert_eq!(frame.tab_bar.width, 118);
        assert_eq!(frame.table.y, 1);
        assert_eq!(frame.status_bar.y, 29);
        assert_eq!(frame.status_bar.height, 1);
        assert_eq!(frame.table.height, 28, "the body gets the rest");
    }

    #[test]
    fn regions_tile_the_area_without_gaps_or_overlap() {
        let frame = compute(area(120, 30));
        let covered = frame.tab_bar.height + frame.table.height + frame.status_bar.height;

        assert_eq!(covered, 30, "every row is accounted for");
        assert_eq!(frame.table.width, 120, "the table takes the whole body");
    }

    /// The border costs a cell on each side, and the table has to be told about it: a
    /// widget drawn over its own frame is how the top row of data disappears.
    #[test]
    fn the_table_is_drawn_inside_the_border() {
        let frame = compute(area(120, 30));

        assert!(frame.bordered());
        let inner = frame.table_inner();
        assert_eq!(inner.x, frame.table.x + 1);
        assert_eq!(inner.y, frame.table.y + 1);
        assert_eq!(inner.width, frame.table.width - 2);
        assert_eq!(inner.height, frame.table.height - 2);
    }

    /// A frame around one row of table is all frame and no table.
    #[test]
    fn the_border_is_dropped_when_there_is_no_room_for_it() {
        let short = compute(area(120, 6));
        assert!(!short.bordered(), "{:?}", short.table);
        assert_eq!(short.table_inner(), short.table);

        let narrow = compute(area(16, 30));
        assert!(!narrow.bordered());
        assert_eq!(narrow.table_inner(), narrow.table);
    }

    /// Chrome is given up before content: a one-row table is useful, a one-row status
    /// bar with no table is not.
    #[test]
    fn short_terminals_drop_chrome_before_content() {
        let three = compute(area(80, 3));
        assert_eq!(three.tab_bar.height, 1);
        assert_eq!(three.table.height, 1);
        assert_eq!(three.status_bar.height, 1);

        let two = compute(area(80, 2));
        assert_eq!(two.status_bar.height, 0, "status bar goes first");
        assert_eq!(two.table.height, 1);

        let one = compute(area(80, 1));
        assert_eq!(one.tab_bar.height, 0, "then the tab bar");
        assert_eq!(one.table.height, 1, "the table always survives");
    }

    /// A layout returning an out-of-bounds rect panics inside ratatui at draw time,
    /// which on the alternate screen means a corrupted terminal rather than an error.
    #[test]
    fn every_region_fits_inside_the_area_at_any_size() {
        for width in [0, 1, 2, 5, 20, 39, 40, 79, 80, 99, 100, 101, 200, 400] {
            for height in [0, 1, 2, 3, 5, 24, 30, 100] {
                let outer = area(width, height);
                let frame = compute(outer);

                for (name, rect) in [
                    ("tab_bar", frame.tab_bar),
                    ("table", frame.table),
                    ("table_inner", frame.table_inner()),
                    ("status_bar", frame.status_bar),
                ] {
                    assert!(
                        contains(outer, rect),
                        "{name} {rect:?} escapes {width}x{height}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_zero_sized_area_yields_zero_sized_regions() {
        for outer in [area(0, 0), area(0, 30), area(120, 0)] {
            let frame = compute(outer);
            assert_eq!(frame.table.width.min(frame.table.height), 0);
            assert!(!frame.bordered());
        }
    }

    /// The layout is offset-aware so it can be used for a sub-region, not just the
    /// whole terminal.
    #[test]
    fn the_layout_respects_a_non_zero_origin() {
        let outer = Rect {
            x: 10,
            y: 5,
            width: 120,
            height: 30,
        };
        let frame = compute(outer);

        assert_eq!(frame.tab_bar.x, 11, "the origin plus a gutter");
        assert_eq!(frame.tab_bar.y, 5);
        assert_eq!(frame.status_bar.y, 34);
        assert!(contains(outer, frame.table));
    }

    #[test]
    fn popups_are_centred_within_their_area() {
        let popup = centred(area(100, 40), 50, 50);

        assert_eq!(popup.width, 50);
        assert_eq!(popup.height, 20);
        assert_eq!(popup.x, 25, "centred horizontally");
        assert_eq!(popup.y, 10);
        assert!(contains(area(100, 40), popup));
    }

    /// A popup larger than the screen panics inside ratatui, and these are percentages
    /// of a terminal whose size nobody controls.
    #[test]
    fn popups_never_escape_their_area() {
        for width in [0, 1, 10, 80, 200] {
            for height in [0, 1, 5, 24, 60] {
                let outer = area(width, height);
                for pct in [0, 50, 100, 200] {
                    assert!(contains(outer, centred(outer, pct, pct)));
                    assert!(contains(outer, centred_at_least(outer, 40, 10, pct)));
                }
            }
        }
    }

    #[test]
    fn a_minimum_sized_popup_grows_to_its_floor_but_not_past_the_area() {
        let roomy = centred_at_least(area(100, 40), 40, 10, 10);
        assert_eq!(roomy.width, 40, "raised from 10% to the minimum");
        assert_eq!(roomy.height, 10);

        let cramped = centred_at_least(area(20, 5), 40, 10, 50);
        assert_eq!(cramped.width, 20, "cannot exceed the area");
        assert_eq!(cramped.height, 5);
    }
}
