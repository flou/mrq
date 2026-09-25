//! Whole-frame render tests: the properties that only hold once every widget is drawn
//! together.
//!
//! The widgets test their own content; what is left over is what the composition can get
//! wrong. Two things, and they are the two that are invisible until a user hits them: a
//! frame that panics at some terminal size, which on the alternate screen means a
//! corrupted terminal rather than an error, and an escape sequence emitted to a terminal
//! that prints escapes instead of obeying them.
//!
//! Nothing here reads the clock or the environment. `Instant`-based state is bypassed by
//! handing the status bar a fixed [`Refresh`], and the timestamps are literals.

use std::time::Instant;

use jiff::Timestamp;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::app::action::{HALF_PAGE_VIEWPORT, Mode, Popup, PopupState, ViewState};
use crate::app::state::Tabs;
use crate::config::keymap::{self, Keymap};
use crate::config::schema::{Column, Config, Filter, Scope, Sort};
use crate::gitlab::model::fixtures::mr;
use crate::gitlab::model::{MergeRequest, Pipeline, PipelineStatus};
use crate::logging::LogBuffer;
use crate::term::caps::{Capabilities, ColorDepth, NotifyEscape};
use crate::ui::statusbar::{Refresh, Status};
use crate::ui::theme::Theme;

fn now() -> Timestamp {
    "2026-09-11T12:00:00Z".parse().unwrap()
}

fn theme(skin: &str, ascii: bool) -> Theme {
    Theme::builtin(
        skin,
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

fn filters() -> Vec<Filter> {
    vec![
        Filter::named("Assigned", Scope::Assigned),
        Filter::named("Reviewing", Scope::ReviewRequested),
        Filter::named("Authored", Scope::Authored),
    ]
}

/// One row per state the table draws differently: approved, selected, draft, conflicted.
fn rows() -> Vec<MergeRequest> {
    let pipeline = |status| {
        Some(Pipeline {
            url: "https://gitlab.example.com/acme/web/web-app/-/pipelines/99".to_owned(),
            status,
            finished_at: None,
        })
    };

    let mut approved = mr("gid://gitlab/MergeRequest/1", "jdoe");
    approved.title = "Fix retry backoff on 5xx".to_owned();
    approved.project_name = "api-gateway".to_owned();
    approved.approved = true;
    approved.approved_by = vec!["me".to_owned()];
    approved.pipeline = pipeline(PipelineStatus::Success);
    approved.web_url = "https://gitlab.example.com/acme/api-gateway/-/merge_requests/1".to_owned();

    let mut conflicted = mr("gid://gitlab/MergeRequest/2", "bwayne");
    conflicted.title = "Bump terraform to 1.9".to_owned();
    conflicted.project_name = "infra".to_owned();
    conflicted.conflicts = true;
    conflicted.pipeline = pipeline(PipelineStatus::Failed);
    conflicted.web_url = "https://gitlab.example.com/acme/infra/-/merge_requests/2".to_owned();

    let mut draft = mr("gid://gitlab/MergeRequest/3", "ckent");
    draft.title = "データベース移行のための変更".to_owned();
    draft.project_name = "docs".to_owned();
    draft.draft = true;
    draft.pipeline = None;
    draft.web_url = "https://gitlab.example.com/acme/docs/-/merge_requests/3".to_owned();

    let mut rows = vec![approved, conflicted, draft];
    for mr in &mut rows {
        mr.recompute_derived("me");
    }
    rows
}

/// Everything [`crate::ui::render`] needs, owned so the borrows can be taken locally.
struct World {
    view: ViewState,
    keymap: Keymap,
    columns: Vec<Column>,
    auth_paused: bool,
}

impl World {
    fn new(skin: &str, ascii: bool) -> Self {
        let config = Config::default();
        let mut view = ViewState {
            tabs: Tabs::new(&filters(), Sort::default(), true, None),
            mode: Mode::Normal,
            wide: false,
            theme: theme(skin, ascii),
            drafts_last: config.sort.drafts_last,
            flash: None,
            log: LogBuffer::new(),
            viewport: HALF_PAGE_VIEWPORT,
        };

        let all = rows();
        let tab = view.tabs.active_mut().expect("a tab");
        tab.apply_rows(all, Instant::now());
        tab.select(Some("gid://gitlab/MergeRequest/2".to_owned()));

        Self {
            view,
            keymap: keymap::resolve(&config.keys).expect("the defaults resolve"),
            columns: config.ui.columns,
            auth_paused: false,
        }
    }

    fn with_popup(mut self, kind: Popup) -> Self {
        self.view.mode = Mode::Popup(PopupState {
            kind,
            cursor: 0,
            query: String::new(),
            lines: vec!["2026-09-11T11:59:48Z  INFO mrq: refreshing Assigned".to_owned()],
            styled: Vec::new(),
            previous_skin: None,
        });
        self
    }

    fn with_auth_paused(mut self) -> Self {
        self.auth_paused = true;
        self
    }

    fn status(&self, rows: &[&MergeRequest], selected: Option<usize>) -> Status {
        let tab = self.view.tabs.active().expect("a tab");
        Status {
            position: selected.map(|index| index + 1),
            count: rows.len(),
            sort_column: tab.sort_column,
            sort_order: tab.sort_order,
            show_drafts: tab.show_drafts(),
            wide: self.view.wide,
            refresh: Refresh::Fresh {
                ago_secs: 12,
                next_in_secs: Some(288),
            },
            auth_paused: self.auth_paused,
            search: self.view.search_text().map(str::to_owned),
            degraded: tab.fragment.lost(),
            dropped: Vec::new(),
            truncated: tab.truncated,
            flash: None,
            spinner: 0,
        }
    }
}

/// Draw one frame and return what the terminal holds, row by row.
fn render(world: &World, width: u16, height: u16, hyperlinks: bool) -> Vec<String> {
    let rows = world.view.visible_rows();
    let tab = world.view.tabs.active().expect("a tab");
    let counts: Vec<usize> = (0..world.view.tabs.len())
        .map(|index| world.view.count_of(index))
        .collect();
    let selected = tab
        .selected_id()
        .and_then(|id| rows.iter().position(|mr| mr.id == id));

    let scene = super::Scene {
        view: &world.view,
        tab,
        rows: &rows,
        counts: &counts,
        status: world.status(&rows, selected),
        keymap: &world.keymap,
        theme: &world.view.theme,
        columns: &world.columns,
        now: now(),
        hyperlinks,
        assignee_trigram: false,
    };

    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| super::render(frame, &scene)).unwrap();

    let buffer = terminal.backend().buffer();
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| {
            (buffer.area.left()..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

/// A dragged window edge passes through every size on the way, and a widget drawn outside
/// its area panics inside ratatui — on the alternate screen, that is a corrupted terminal
/// rather than an error message.
#[test]
fn the_frame_renders_at_every_size_skin_and_mode() {
    for (skin, ascii) in [
        ("catppuccin-mocha", false),
        ("nord", false),
        ("catppuccin-mocha", true),
    ] {
        for mode in [
            None,
            Some(Popup::Help),
            Some(Popup::Sort),
            Some(Popup::Filter),
            Some(Popup::Skin),
            Some(Popup::Log),
        ] {
            let world = match mode {
                Some(kind) => World::new(skin, ascii).with_popup(kind),
                None => World::new(skin, ascii),
            };

            for width in [1u16, 2, 5, 19, 20, 40, 80, 120, 200] {
                for height in [1u16, 2, 3, 4, 5, 24, 60] {
                    let lines = render(&world, width, height, false);
                    assert_eq!(lines.len(), usize::from(height));
                }
            }
        }
    }
}

/// The auth-pause segment must render at width extremes without panicking, and its own
/// text must say nothing the ascii theme cannot draw.
///
/// Only the status bar (the last line) is checked for ascii-ness: the table above it
/// carries a deliberately non-ascii title fixture, and user content is exempt from the
/// ascii-theme rule that governs glyphs the theme itself emits.
#[test]
fn the_auth_pause_segment_renders_safely_in_every_mode() {
    let world = World::new("catppuccin-mocha", true).with_auth_paused();

    for width in [1u16, 20, 80, 200] {
        for height in [1u16, 24] {
            let lines = render(&world, width, height, false);
            assert_eq!(lines.len(), usize::from(height));
        }
    }

    let lines = render(&world, 120, 24, false);
    let status_bar = lines.last().expect("at least one line");
    assert!(
        status_bar.is_ascii(),
        "non-ascii in the ascii theme: `{status_bar}`"
    );
}

/// The frame must carry no escape sequence unless the terminal said it understands one:
/// an unsupported terminal prints them rather than ignoring them.
#[test]
fn the_frame_carries_escapes_only_when_hyperlinks_are_supported() {
    let world = World::new("catppuccin-mocha", false);

    let plain = render(&world, 120, 24, false).join("");
    assert!(!plain.contains('\x1b'), "no terminal support, no escapes");

    let linked = render(&world, 120, 24, true).join("");
    assert!(
        linked.contains("\x1b]8;id="),
        "titles should be linked when the terminal advertises OSC 8"
    );
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

/// A popup opening and then closing must never leave a hyperlink open on the real
/// terminal.
///
/// The popup is drawn *after* the table, so it can cover only part of a linked title —
/// say, the cell that opens the link, but not the one that closes it. `Terminal::draw`
/// only sends cells whose content changed since the last frame; if the closing cell's
/// content happens to be unchanged, it is never resent. A freshly reopened link with no
/// resent close reads, to the real terminal, as still open — everything printed after it,
/// on every following row and frame, becomes part of that one link. This drives an actual
/// `CrosstermBackend` through open, popup-up, and popup-closed, and checks the opens and
/// closes it wrote balance.
#[test]
fn a_popup_opening_and_closing_never_leaves_a_hyperlink_open() {
    use ratatui::Viewport;
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::Rect;

    let area = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 24,
    };
    let sink = Sink::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(sink.clone()),
        ratatui::TerminalOptions {
            viewport: Viewport::Fixed(area),
        },
    )
    .unwrap();

    let draw = |terminal: &mut Terminal<CrosstermBackend<Sink>>, world: &World| {
        let rows = world.view.visible_rows();
        let tab = world.view.tabs.active().expect("a tab");
        let counts: Vec<usize> = (0..world.view.tabs.len())
            .map(|index| world.view.count_of(index))
            .collect();
        let selected = tab
            .selected_id()
            .and_then(|id| rows.iter().position(|mr| mr.id == id));
        let scene = super::Scene {
            view: &world.view,
            tab,
            rows: &rows,
            counts: &counts,
            status: world.status(&rows, selected),
            keymap: &world.keymap,
            theme: &world.view.theme,
            columns: &world.columns,
            now: now(),
            hyperlinks: true,
            assignee_trigram: false,
        };
        terminal.draw(|frame| super::render(frame, &scene)).unwrap();
    };

    let mut world = World::new("catppuccin-mocha", false);
    draw(&mut terminal, &world);

    world.view.mode = Mode::Popup(PopupState {
        kind: Popup::Help,
        cursor: 0,
        query: String::new(),
        lines: Vec::new(),
        styled: Vec::new(),
        previous_skin: None,
    });
    draw(&mut terminal, &world);

    world.view.mode = Mode::Normal;
    draw(&mut terminal, &world);

    let written = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let opens = written.matches("\x1b]8;id=").count();
    let closes = written.matches("\x1b]8;;\x1b\\").count();
    assert_eq!(
        opens, closes,
        "an opened hyperlink was never followed by a matching close: {opens} opens, \
         {closes} closes:\n{written:?}"
    );
}

/// A skin change must repaint every cell: an unpainted cell that keeps the old theme's
/// background is exactly the mismatched strip a user sees around the widgets.
#[test]
fn a_skin_change_repaints_every_cell_in_the_new_background() {
    use ratatui::style::Color;

    let base_of = |skin: &str| {
        let base = crate::ui::skins::palette(skin)
            .expect("a built-in skin")
            .base;
        Color::Rgb(base.r, base.g, base.b)
    };

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // The empty stretch of body below the last row: nothing is drawn there, so its
    // background is whatever the frame painted behind the widgets.
    let empty_body = |terminal: &Terminal<TestBackend>| -> Vec<Color> {
        let buffer = terminal.backend().buffer();
        (6..22)
            .flat_map(|y| (1..79).map(move |x| buffer[(x, y)].bg))
            .collect()
    };

    let mut world = World::new("catppuccin-mocha", false);
    let (rows, counts, selected) = scene_parts(&world);
    let scene = super::Scene {
        view: &world.view,
        tab: world.view.tabs.active().unwrap(),
        rows: &rows,
        counts: &counts,
        status: world.status(&rows, selected),
        keymap: &world.keymap,
        theme: &world.view.theme,
        columns: &world.columns,
        now: now(),
        hyperlinks: false,
        assignee_trigram: false,
    };
    terminal.draw(|frame| super::render(frame, &scene)).unwrap();
    let dark = empty_body(&terminal);
    assert!(
        dark.iter().all(|bg| *bg == base_of("catppuccin-mocha")),
        "the unused body starts in the dark background"
    );

    world.view.theme = world.view.theme.with_skin("nord");
    let (rows, counts, selected) = scene_parts(&world);
    let scene = super::Scene {
        view: &world.view,
        tab: world.view.tabs.active().unwrap(),
        rows: &rows,
        counts: &counts,
        status: world.status(&rows, selected),
        keymap: &world.keymap,
        theme: &world.view.theme,
        columns: &world.columns,
        now: now(),
        hyperlinks: false,
        assignee_trigram: false,
    };
    terminal.draw(|frame| super::render(frame, &scene)).unwrap();
    let other = empty_body(&terminal);
    assert!(
        other.iter().all(|bg| *bg == base_of("nord")),
        "the very same cells repainted in the new background"
    );

    assert_ne!(dark, other, "the two skins differ");
}

/// `Clear` hard-resets a popup's area to the terminal's own default colour before the
/// panel is drawn over it, so the panel has to repaint its own background explicitly —
/// otherwise a skin preview updates every other colour but leaves the popup stuck on
/// whatever background `Clear` left behind.
#[test]
fn previewing_a_skin_repaints_the_popup_background() {
    use ratatui::style::Color;

    let base_of = |skin: &str| {
        let base = crate::ui::skins::palette(skin)
            .expect("a built-in skin")
            .base;
        Color::Rgb(base.r, base.g, base.b)
    };

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut world = World::new("catppuccin-mocha", false).with_popup(Popup::Skin);

    let draw = |world: &World, terminal: &mut Terminal<TestBackend>| {
        let (rows, counts, selected) = scene_parts(world);
        let scene = super::Scene {
            view: &world.view,
            tab: world.view.tabs.active().unwrap(),
            rows: &rows,
            counts: &counts,
            status: world.status(&rows, selected),
            keymap: &world.keymap,
            theme: &world.view.theme,
            columns: &world.columns,
            now: now(),
            hyperlinks: false,
            assignee_trigram: false,
        };
        terminal.draw(|frame| super::render(frame, &scene)).unwrap();
        // The centre of an 80x24 frame sits well inside the popup's 70%-by-70% area.
        terminal.backend().buffer()[(40, 12)].bg
    };

    let dark = draw(&world, &mut terminal);
    assert_eq!(
        dark,
        base_of("catppuccin-mocha"),
        "starts in the dark background"
    );

    world.view.theme = world.view.theme.with_skin("nord");
    let other = draw(&world, &mut terminal);
    assert_eq!(other, base_of("nord"), "the preview repaints the popup too");
}

/// The parts of a scene that borrow from `world`, so the borrows end before the next
/// scene is built.
fn scene_parts(world: &World) -> (Vec<&MergeRequest>, Vec<usize>, Option<usize>) {
    let rows = world.view.visible_rows();
    let counts: Vec<usize> = (0..world.view.tabs.len())
        .map(|index| world.view.count_of(index))
        .collect();
    let selected = world
        .view
        .tabs
        .active()
        .unwrap()
        .selected_id()
        .and_then(|id| rows.iter().position(|mr| mr.id == id));
    (rows, counts, selected)
}
