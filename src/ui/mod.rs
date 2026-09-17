//! Rendering: everything drawn on screen, and nothing else.
//!
//! Widgets are pure functions of `(&State, Rect, &Theme)` — they never fetch
//! and never mutate application state. Network access belongs to `gitlab`; the escape
//! sequences the widgets need are built in `term` and only placed here.
//!
//! Contents:
//!
//! - `layout`    — the three frame regions and popup placement
//! - `columns`   — the column model and responsive width allocation
//! - `table`     — the merge-request table and row state styling
//! - `statusbar` — position, sort, refresh state and transient flashes
//! - `tabbar`    — the filter tabs with overflow and error markers
//! - `popup`     — help, sort menu, filter switcher, skin picker and log overlays
//! - `theme`     — roles to colours and glyphs at three colour depths
//! - `palette`   — the 25 named swatches a skin is made of
//! - `skins`     — the built-in palettes and their aliases
//!
//! # Invariant
//!
//! No literal colour or glyph appears outside `theme`, `palette` and `skins`. Inlining one
//! silently rots the ascii, light and 16-colour paths, which nothing else will catch.

pub mod columns;
pub mod layout;

pub mod palette;
pub mod popup;
pub mod skins;
pub mod statusbar;
pub mod tabbar;
pub mod table;
pub mod theme;

#[cfg(test)]
mod frame;

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

use crate::app::action::ViewState;
use crate::app::state::Tab;
use crate::config::keymap::Keymap;
use crate::config::schema::Column;
use crate::gitlab::model::MergeRequest;
use crate::ui::statusbar::Status;
use crate::ui::theme::{Role, Theme};

/// Everything one frame needs, gathered before `Terminal::draw` takes the terminal.
///
/// Borrowed rather than owned: the alternative is cloning every merge request in every
/// tab once per frame. Assembling it is the caller's job, which is what
/// lets [`render`] be driven by a test backend as well as by a real terminal.
pub struct Scene<'a> {
    pub view: &'a ViewState,
    pub tab: &'a Tab,
    /// The active tab's rows, already filtered and sorted.
    pub rows: &'a [&'a MergeRequest],
    /// Row count per tab, in tab order, for the tab bar.
    pub counts: &'a [usize],
    /// `dropped` is filled in by [`render`], once the table's width is known.
    pub status: Status,
    pub keymap: &'a Keymap,
    pub theme: &'a Theme,
    pub columns: &'a [Column],
    pub now: jiff::Timestamp,
    /// Whether the terminal advertises OSC 8, so titles can be made clickable.
    pub hyperlinks: bool,
}

/// The panel title: which filter the rows below belong to, and how many there are.
fn panel_title<'a>(scene: &Scene<'_>) -> Line<'a> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(scene.tab.name.clone(), scene.theme.style(Role::Header)),
        Span::styled(
            format!("({}) ", scene.rows.len()),
            scene.theme.style(Role::Dim),
        ),
    ])
}

/// Draw one frame.
pub fn render(frame: &mut Frame, scene: &Scene<'_>) {
    let regions = layout::compute(frame.area());

    // Fill the entire frame with the theme's background colour before drawing any widget,
    // so that a skin change does not leave the terminal's native background peeking through
    // around the edges of widgets.
    let base = scene.theme.base_colour();
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(base)), area);

    let table_area = regions.table_inner();
    let allocation = table::allocate(scene.columns, table_area.width);

    let bar = tabbar::build(
        &scene.view.tabs,
        scene.counts,
        scene.theme,
        regions.tab_bar.width,
    );
    frame.render_widget(bar, regions.tab_bar);

    if regions.bordered() {
        frame.render_widget(scene.theme.panel(panel_title(scene)), regions.table);
    }

    let widget = table::build(
        table::windowed(
            scene.rows,
            scene.tab.scroll,
            table::visible_row_count(table_area),
        ),
        scene.tab,
        &allocation,
        scene.theme,
        scene.now,
    );
    frame.render_widget(widget, table_area);

    if scene.hyperlinks {
        table::link_titles(
            frame.buffer_mut(),
            table_area,
            table::windowed(
                scene.rows,
                scene.tab.scroll,
                table::visible_row_count(table_area),
            ),
            &allocation,
        );
    }

    // Built here rather than by the caller because the dropped columns are only known
    // once the table's width is.
    let mut status = scene.status.clone();
    status.dropped = allocation.dropped.clone();
    let line = statusbar::build(&status, scene.theme, regions.status_bar.width);
    frame.render_widget(line, regions.status_bar);

    // Last, over everything: a popup is modal, and `Clear` is what stops the table
    // showing through its borders.
    let region = popup::area(regions.body());
    if let Some((clear, widget)) =
        popup::build(scene.view, scene.keymap, scene.theme, scene.now, region)
    {
        frame.render_widget(clear, region);
        frame.render_widget(widget, region);
    }
}
