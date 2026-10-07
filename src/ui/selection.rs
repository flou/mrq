//! Text selection inside a text popup, bounded to the popup's content rectangle.
//!
//! With the mouse captured the terminal no longer selects anything itself, and its own
//! selection runs across the whole screen regardless of the popup on top. This one lives
//! in popup content coordinates — `(line, column)` into the rendered lines — so every
//! point is clamped to the content rectangle and a drag that leaves the popup stops at
//! its edge.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthChar;

use crate::ui::markdown::StyledLine;

/// A point in the popup's text: an absolute line index and a display column.
pub type Point = (usize, usize);

/// A drag from `anchor` to `head`, either of which may come first in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
}

/// The first line shown by a popup scrolled to `cursor`, mirroring the window the popup
/// draws: the cursor is the top line, held where the last page still fills `height`.
pub const fn top_line(cursor: usize, total: usize, height: usize) -> usize {
    if total <= height {
        0
    } else {
        let last = total - height;
        if cursor < last { cursor } else { last }
    }
}

/// The text point under screen cell `(x, y)`, clamped into `content`.
///
/// `None` for an empty popup or an empty rectangle.
pub fn point_at(content: Rect, top: usize, total: usize, x: u16, y: u16) -> Option<Point> {
    if content.width == 0 || content.height == 0 || total == 0 {
        return None;
    }
    let x = x.clamp(content.left(), content.right() - 1);
    let y = y.clamp(content.top(), content.bottom() - 1);
    let line = (top + usize::from(y - content.top())).min(total - 1);
    Some((line, usize::from(x - content.left())))
}

impl Selection {
    /// Whether the drag covers anything: a click leaves anchor and head equal.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The display-column range `[from, to)` the selection covers on `line`, if any.
    /// The last point's cell is included, as in a terminal's own selection.
    fn span(&self, line: usize, width: usize) -> Option<(usize, usize)> {
        let (start, end) = self.ordered();
        if line < start.0 || line > end.0 {
            return None;
        }
        let from = if line == start.0 { start.1 } else { 0 };
        let to = if line == end.0 { end.1 + 1 } else { width };
        let to = to.min(width);
        (from < to).then_some((from, to))
    }

    /// The selected text, lines joined by newlines, trailing blanks trimmed per line.
    pub fn text(&self, lines: &[StyledLine]) -> String {
        let (start, end) = self.ordered();
        let mut out = Vec::new();
        for index in start.0..=end.0.min(lines.len().saturating_sub(1)) {
            let Some(line) = lines.get(index) else { break };
            let plain: String = line.iter().map(|s| s.text.as_str()).collect();
            let width = display_width(&plain);
            let piece = self
                .span(index, width)
                .map(|(from, to)| slice(&plain, from, to))
                .unwrap_or_default();
            out.push(piece.trim_end().to_owned());
        }
        out.join("\n")
    }

    /// Reverse the selected cells of the lines currently on screen.
    pub fn highlight(&self, buffer: &mut Buffer, content: Rect, top: usize, lines: &[StyledLine]) {
        for row in 0..usize::from(content.height) {
            let index = top + row;
            let Some(line) = lines.get(index) else { break };
            let width = line.iter().map(|s| display_width(&s.text)).sum();
            let Some((from, to)) = self.span(index, width) else {
                continue;
            };
            let y = content.top() + u16::try_from(row).unwrap_or(u16::MAX);
            for column in from..to.min(usize::from(content.width)) {
                let x = content.left() + u16::try_from(column).unwrap_or(u16::MAX);
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}

fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// The characters of `text` that start in display columns `[from, to)`.
fn slice(text: &str, from: usize, to: usize) -> String {
    let mut column = 0;
    let mut out = String::new();
    for c in text.chars() {
        if column >= from && column < to {
            out.push(c);
        }
        column += c.width().unwrap_or(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::markdown::Segment;
    use crate::ui::theme::Role;

    fn lines(text: &[&str]) -> Vec<StyledLine> {
        text.iter()
            .map(|t| {
                vec![Segment {
                    role: Role::Normal,
                    text: (*t).to_owned(),
                }]
            })
            .collect()
    }

    #[test]
    fn points_are_clamped_into_the_content_rectangle() {
        let content = Rect::new(10, 5, 20, 4);
        assert_eq!(point_at(content, 3, 100, 0, 0), Some((3, 0)));
        assert_eq!(point_at(content, 3, 100, 200, 200), Some((6, 19)));
        assert_eq!(point_at(content, 3, 5, 15, 8), Some((4, 5)));
        assert_eq!(point_at(content, 0, 0, 15, 8), None);
    }

    #[test]
    fn the_window_stops_at_the_last_full_page() {
        assert_eq!(top_line(2, 10, 4), 2);
        assert_eq!(top_line(9, 10, 4), 6);
        assert_eq!(top_line(9, 3, 4), 0);
    }

    #[test]
    fn a_selection_spans_lines_in_either_direction() {
        let text = lines(&["hello world", "second", "third line"]);
        let forward = Selection {
            anchor: (0, 6),
            head: (2, 4),
        };
        let backward = Selection {
            anchor: (2, 4),
            head: (0, 6),
        };
        assert_eq!(forward.text(&text), "world\nsecond\nthird");
        assert_eq!(backward.text(&text), forward.text(&text));
    }

    #[test]
    fn a_single_line_selection_includes_both_end_cells() {
        let text = lines(&["abcdef"]);
        let selection = Selection {
            anchor: (0, 1),
            head: (0, 3),
        };
        assert_eq!(selection.text(&text), "bcd");
    }

    #[test]
    fn highlight_reverses_only_the_selected_cells() {
        let text = lines(&["abcdef", "ghij"]);
        let content = Rect::new(2, 1, 10, 2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 5));
        Selection {
            anchor: (0, 4),
            head: (1, 1),
        }
        .highlight(&mut buffer, content, 0, &text);
        let reversed = |x, y| {
            buffer
                .cell((x, y))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        };
        assert!(!reversed(2 + 3, 1));
        assert!(reversed(2 + 4, 1) && reversed(2 + 5, 1));
        assert!(!reversed(2 + 6, 1));
        assert!(reversed(2, 2) && reversed(3, 2));
        assert!(!reversed(4, 2));
    }
}
