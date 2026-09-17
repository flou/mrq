//! The filter tab bar.
//!
//! One tab per configured filter, each showing the number of rows that
//! filter is currently displaying, the `1`..`9` position that jumps to it, and a marker
//! when it needs attention.
//!
//! # Overflow keeps the active tab
//!
//! More filters can be configured than fit the width, and the obvious implementation —
//! render until the room runs out — hides the active tab the moment the user navigates
//! past the fold, which reads as the tab bar being broken. The visible window is
//! therefore chosen to contain the active tab, with `…` on whichever side is elided and
//! a count of what is hidden, so the bar stays a map of where you are rather than a
//! prefix of the filter list.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::state::{Attention, Tabs};
use crate::ui::theme::{Role, Theme};

/// One tab's rendered parts and how they should be styled.
///
/// Kept apart rather than formatted into one string because each is coloured
/// independently: the shortcut says which key, the name says which filter, the count says
/// how much is in it, and the marker says how bad.
struct Entry {
    shortcut: String,
    name: String,
    count: String,
    marker: &'static str,
    attention: Attention,
    active: bool,
}

impl Entry {
    /// Including the padding that makes the active tab a bar rather than bare text, and
    /// the divider after it. Counting a divider for the last tab too overestimates by one
    /// cell, which only ever makes the window fit more conservatively.
    fn width(&self) -> usize {
        1 + self.shortcut.width()
            + self.name.width()
            + 1
            + self.count.width()
            + self.marker.width()
            + 2
    }
}

/// The `1`..`9` shortcut prefix, or nothing for a tab no number addresses.
///
/// Beyond nine there is no key to advertise, and inventing `10:` would promise a
/// shortcut that does not exist.
fn shortcut(index: usize) -> String {
    match index {
        0..9 => format!("{}:", index + 1),
        _ => String::new(),
    }
}

fn entries(tabs: &Tabs, counts: &[usize], theme: &Theme) -> Vec<Entry> {
    tabs.iter()
        .enumerate()
        .map(|(index, tab)| {
            let count = counts.get(index).copied().unwrap_or(0);
            let attention = tab.attention();
            Entry {
                shortcut: shortcut(index),
                name: tab.name.clone(),
                count: count.to_string(),
                marker: theme.attention_marker(attention),
                attention,
                active: index == tabs.active_index(),
            }
        })
        .collect()
}

/// The slice of tabs to show, as `(start, end)`, always containing the active one.
fn window(entries: &[Entry], active: usize, width: usize) -> (usize, usize) {
    if entries.is_empty() {
        return (0, 0);
    }

    // Grow rightwards from the active tab first, then leftwards: the common case is a
    // handful of filters where everything fits and this returns the whole range.
    let mut start = active;
    let mut end = active + 1;
    let mut used = entries[active].width();

    loop {
        let grew_right = end < entries.len() && used + entries[end].width() <= width;
        if grew_right {
            used += entries[end].width();
            end += 1;
        }
        let grew_left = start > 0 && used + entries[start - 1].width() <= width;
        if grew_left {
            start -= 1;
            used += entries[start].width();
        }
        if !grew_right && !grew_left {
            return (start, end);
        }
    }
}

/// Build the tab bar for one frame.
///
/// `counts` is the number of rows each filter currently shows — after its draft toggle
/// and search, not the raw fetched total, because that is the number the tab is a label
/// for.
pub fn build<'a>(tabs: &Tabs, counts: &[usize], theme: &Theme, width: u16) -> Line<'a> {
    let width = usize::from(width);
    let entries = entries(tabs, counts, theme);
    if entries.is_empty() || width == 0 {
        return Line::default();
    }

    // The `…` and `…+N` markers occupy width of their own. Budgeting for both even when
    // only one ends up shown costs a couple of cells and removes the circularity in
    // sizing a window whose markers depend on the window.
    let total: usize = entries.iter().map(Entry::width).sum();
    let reserve = if total <= width {
        0
    } else {
        2 * theme.ellipsis().width() + 1 + entries.len().to_string().width()
    };

    let (start, end) = window(&entries, tabs.active_index(), width.saturating_sub(reserve));
    let hidden = start + (entries.len() - end);
    let mut spans: Vec<Span<'a>> = Vec::new();

    if start > 0 {
        spans.push(Span::styled(
            theme.ellipsis().to_owned(),
            theme.style(Role::Dim),
        ));
    }

    for (offset, entry) in entries[start..end].iter().enumerate() {
        if offset > 0 {
            spans.push(Span::styled(
                theme.tab_divider().to_owned(),
                theme.style(Role::Border),
            ));
        }

        // The highlight is the tab: it runs under every part, and each part patches its
        // own foreground over it. On a terminal with no highlight to give, that base is
        // reverse video and the same layering still reads.
        let base = if entry.active {
            theme.style(Role::Selection)
        } else {
            Style::default()
        };
        let part = |role: Role| base.patch(theme.style(role));
        let (shortcut_role, name_role) = if entry.active {
            (Role::Accent, Role::Header)
        } else {
            (Role::Dim, Role::Normal)
        };

        spans.push(Span::styled(" ".to_owned(), base));
        spans.push(Span::styled(entry.shortcut.clone(), part(shortcut_role)));
        spans.push(Span::styled(entry.name.clone(), part(name_role)));
        spans.push(Span::styled(" ".to_owned(), base));
        spans.push(Span::styled(entry.count.clone(), part(Role::Dim)));
        if !entry.marker.is_empty() {
            spans.push(Span::styled(
                entry.marker.to_owned(),
                part(theme.attention_role(entry.attention)),
            ));
        }
        spans.push(Span::styled(" ".to_owned(), base));
    }

    if hidden > 0 {
        spans.push(Span::styled(
            format!("{}+{hidden}", theme.ellipsis()),
            theme.style(Role::Dim),
        ));
    }

    Line::from(clip(spans, width))
}

/// Cut a span list to a display width, keeping each span's style.
///
/// A backstop rather than the main mechanism: [`window`] already sizes the bar. It is
/// what makes the width bound hold at sizes where nothing fits — a terminal three cells
/// wide still has to render *something*, and a `Line` wider than its `Rect` is a panic
/// inside ratatui, which on the alternate screen means a corrupted terminal.
fn clip<'a>(spans: Vec<Span<'a>>, width: usize) -> Vec<Span<'a>> {
    let mut out = Vec::new();
    let mut used = 0usize;

    for span in spans {
        let span_width = span.content.width();
        if used + span_width <= width {
            used += span_width;
            out.push(span);
            continue;
        }

        let head = crate::ui::table::truncate(&span.content, width - used, "");
        if !head.is_empty() {
            out.push(Span::styled(head, span.style));
        }
        break;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Filter, Scope, Sort};
    use crate::gitlab::model::fixtures::mr;
    use crate::gitlab::query::Fragment;
    use crate::term::caps::{Capabilities, ColorDepth, NotifyEscape};
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

    fn tabs(names: &[&str]) -> Tabs {
        let filters: Vec<Filter> = names
            .iter()
            .map(|name| Filter::named(name, Scope::Assigned))
            .collect();
        Tabs::new(&filters, Sort::default(), true, None)
    }

    fn rendered(tabs: &Tabs, counts: &[usize], width: u16) -> String {
        build(tabs, counts, &theme(), width).to_string()
    }

    #[test]
    fn every_filter_gets_a_tab_with_its_visible_count() {
        let tabs = tabs(&["Assigned", "Reviewing", "Authored"]);
        let line = rendered(&tabs, &[12, 3, 0], 200);

        assert!(line.contains("Assigned 12"), "{line}");
        assert!(line.contains("Reviewing 3"), "{line}");
        assert!(line.contains("Authored 0"), "{line}");
    }

    /// The number keys are only discoverable if the bar says which is which.
    #[test]
    fn the_first_nine_tabs_advertise_their_shortcut() {
        let names: Vec<String> = (1..=11).map(|i| format!("F{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let tabs = tabs(&refs);
        let line = rendered(&tabs, &[0; 11], 400);

        assert!(line.contains("1:F1"), "{line}");
        assert!(line.contains("9:F9"), "{line}");
        assert!(
            !line.contains("10:F10"),
            "no key jumps to the tenth: {line}"
        );
        assert!(line.contains("F10"), "but it is still listed: {line}");
    }

    #[test]
    fn the_active_tab_is_styled_differently() {
        let mut tabs = tabs(&["One", "Two"]);
        tabs.activate(1);
        let line = build(&tabs, &[0, 0], &theme(), 200);

        let active = line
            .spans
            .iter()
            .find(|s| s.content.contains("Two"))
            .expect("the active tab is rendered");
        let inactive = line
            .spans
            .iter()
            .find(|s| s.content.contains("One"))
            .expect("the inactive tab is rendered");

        assert_ne!(active.style, inactive.style);
    }

    /// A tab whose fetch went wrong is marked, and a degraded one is
    /// marked differently — one means fields are missing, the other means the list may
    /// be wrong.
    #[test]
    fn problem_and_degraded_tabs_carry_different_markers() {
        let mut tabs = tabs(&["Healthy", "Degraded", "Broken"]);
        tabs.get_mut(1).unwrap().fragment = Fragment::minimal();
        tabs.get_mut(2).unwrap().partial = true;

        let line = rendered(&tabs, &[1, 1, 1], 200);

        assert!(line.contains("Healthy 1 "), "no marker: {line}");
        assert!(line.contains("Degraded 1~"), "{line}");
        assert!(line.contains("Broken 1!"), "{line}");
    }

    /// A failed fetch outranks a degraded query: the list being wrong is the worse news.
    #[test]
    fn a_failed_fetch_outranks_a_degraded_query() {
        let mut tabs = tabs(&["One"]);
        let tab = tabs.get_mut(0).unwrap();
        tab.fragment = Fragment::minimal();
        tab.apply_error(&crate::error::Error::Other("boom".into()));

        assert_eq!(tab.attention(), Attention::Problem);
    }

    /// The marker keeps its own colour on an inactive tab, where nothing else carries
    /// the severity.
    #[test]
    fn the_marker_is_coloured_independently_of_the_label() {
        let mut tabs = tabs(&["One", "Two"]);
        tabs.get_mut(1).unwrap().partial = true;
        let line = build(&tabs, &[0, 0], &theme(), 200);

        let marker = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "!")
            .expect("the marker is its own span");
        assert_eq!(marker.style, theme().style(Role::Failure));
    }

    /// `…` and a count, not a label cut in half.
    #[test]
    fn overflow_reports_how_many_tabs_are_hidden() {
        let tabs = tabs(&["Alpha", "Bravo", "Charlie", "Delta", "Echo"]);
        let line = rendered(&tabs, &[0; 5], 24);

        assert!(line.contains('+'), "a count of the hidden tabs: {line}");
        assert!(
            line.width() <= 24,
            "`{line}` is {} cells wide",
            line.width()
        );
    }

    /// Rendering a prefix of the filter list would hide the tab the user is on the
    /// moment they navigate past the fold.
    #[test]
    fn the_active_tab_is_always_visible() {
        let names: Vec<String> = (1..=9).map(|i| format!("Filter{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        for active in 0..9 {
            let mut tabs = tabs(&refs);
            tabs.activate(active);
            let line = rendered(&tabs, &[0; 9], 30);

            assert!(
                line.contains(&format!("Filter{}", active + 1)),
                "tab {active} is off the bar: {line}"
            );
            assert!(line.width() <= 30, "`{line}` overflows");
        }
    }

    #[test]
    fn a_window_that_starts_late_is_marked_on_both_sides() {
        let names: Vec<String> = (1..=9).map(|i| format!("Filter{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut tabs = tabs(&refs);
        tabs.activate(4);

        let line = rendered(&tabs, &[0; 9], 30);
        let ellipsis = theme().ellipsis();
        assert!(line.starts_with(ellipsis), "elided on the left: {line}");
        assert!(
            line.contains(&format!("{ellipsis}+")),
            "and counted: {line}"
        );
    }

    /// A dragged window edge passes through every width, including the useless ones.
    #[test]
    fn degenerate_widths_do_not_panic() {
        let tabs = tabs(&["Assigned", "Reviewing"]);
        for width in 0..40 {
            let line = rendered(&tabs, &[5, 5], width);
            assert!(
                line.width() <= usize::from(width),
                "width {width}: `{line}` is {} cells",
                line.width()
            );
        }
    }

    #[test]
    fn no_filters_is_an_empty_bar() {
        assert_eq!(rendered(&tabs(&[]), &[], 80), "");
    }

    /// The count is what the tab is showing, not what was fetched: a draft toggle or a
    /// search changes the number the label is a label for.
    #[test]
    fn counts_are_supplied_by_the_caller_not_read_from_the_tab() {
        let mut tabs = tabs(&["One"]);
        tabs.get_mut(0)
            .unwrap()
            .apply_rows(vec![mr("a", "x"), mr("b", "x")], Instant::now());

        let line = rendered(&tabs, &[1], 80);
        assert!(line.contains("One 1"), "the visible count wins: {line}");
    }
}
