//! The status bar.
//!
//! //!
//! ```text
//! {position}/{count} · sort: {COLUMN} {↑|↓} · drafts: {shown|hidden} · {refresh state} · {search} · [?] help
//! ```
//!
//! # Everything non-fatal surfaces here
//!
//! Staleness, backoff, auth failure, a degraded query, columns the width forced out — none
//! of these stop the program, and none of them belong in a popup the user has to go and
//! open. They are one line, always visible, so the table can be trusted to mean what it
//! says: if something is missing from it, this line says so.
//!
//! # The countdown is told, not computed
//!
//! `next in {rel}` is whatever the scheduler decided — `interval + jitter` on success, an
//! exponential backoff on failure. Recomputing it here would produce a
//! confident number that disagrees with reality, so the scheduler publishes its deadline
//! and this module only formats it.

use std::time::Instant;

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::state::{FetchState, Tab};
use crate::config::schema::{Column, Order};
use crate::ui::table::compact_seconds;
use crate::ui::theme::{Role, Theme};

/// The six refresh states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refresh {
    /// Nothing fetched yet and nothing in flight.
    Waiting,
    /// A request is in flight.
    Fetching,
    /// The last fetch succeeded.
    Fresh {
        ago_secs: i64,
        next_in_secs: Option<i64>,
    },
    /// Rows restored from the cache, with no live fetch behind them yet.
    ///
    /// Distinct from `Stale`, which means a fetch *failed*: nothing is wrong here, the
    /// data is just from last time. Saying "retrying" over a warm start would invent a
    /// failure, and saying "refreshed 3h ago" would claim this session fetched it.
    Cached { age_secs: i64, refreshing: bool },
    /// The last fetch failed but a previous snapshot is still on screen.
    Stale { retry_in_secs: Option<i64> },
    /// The last fetch failed and there is nothing to show.
    Offline { error: String },
}

/// Derive the refresh state from a tab and the clock.
///
/// The split between `Stale` and `Offline` is whether the user is looking at real data:
/// a failure over a previous snapshot is stale, a failure over nothing is offline. Saying
/// "stale" above an empty table would imply there is something there to distrust.
pub fn refresh_of(tab: &Tab, now: Instant) -> Refresh {
    let until = |deadline: Option<Instant>| {
        deadline.map(|at| at.saturating_duration_since(now).as_secs() as i64)
    };

    // Whether the rows on screen came from the cache and nothing has replaced them yet.
    // The marker must hold *while* the first live fetch runs, so it outranks the
    // spinner: a spinner alone would imply the rows are this session's.
    let warm = tab.cached_age().filter(|_| tab.fetched_at.is_none());

    match &tab.state {
        FetchState::Fetching => match warm {
            Some(age) => Refresh::Cached {
                age_secs: age.as_secs() as i64,
                refreshing: true,
            },
            None => Refresh::Fetching,
        },
        FetchState::Idle => match warm {
            Some(age) => Refresh::Cached {
                age_secs: age.as_secs() as i64,
                refreshing: false,
            },
            None => Refresh::Waiting,
        },
        FetchState::Loaded => Refresh::Fresh {
            ago_secs: tab
                .fetched_at
                .map(|at| now.saturating_duration_since(at).as_secs() as i64)
                .unwrap_or(0),
            next_in_secs: until(tab.next_refresh),
        },
        FetchState::Failed { message } => {
            // Cached rows count as something to show: "offline" over a populated table
            // would tell the user there is nothing there.
            if tab.fetched_at.is_some() || warm.is_some() {
                Refresh::Stale {
                    retry_in_secs: until(tab.next_refresh),
                }
            } else {
                Refresh::Offline {
                    error: message.clone(),
                }
            }
        }
    }
}

/// Everything the status bar renders, gathered by the caller.
///
/// A struct rather than a long argument list, and owned rather than borrowed, so the
/// draw closure does not hold a borrow of the application while ratatui holds the frame.
#[derive(Debug, Clone)]
pub struct Status {
    /// 1-based row position, or `None` when nothing is selected.
    pub position: Option<usize>,
    pub count: usize,
    pub sort_column: Column,
    pub sort_order: Order,
    pub show_drafts: bool,
    /// Wide mode (`w`), shown only when on — unlike `show_drafts`, which always names
    /// its state. Wide mode's default state needs no explanation; drafts hidden does.
    pub wide: bool,
    pub refresh: Refresh,
    /// A runtime 401/403 has stopped every filter. Orthogonal to `refresh`: rows
    /// can be fresh, cached or stale independently of whether the credential works.
    pub auth_paused: bool,
    pub search: Option<String>,
    /// What a degraded query gave up, from `Fragment::lost`.
    pub degraded: Option<String>,
    /// Columns the terminal width forced out.
    pub dropped: Vec<Column>,
    /// The list was capped at `max_results`.
    pub truncated: bool,
    /// A transient message, already checked for expiry.
    pub flash: Option<String>,
    /// Which spinner frame to show, advanced by the caller's clock.
    pub spinner: usize,
}

/// A segment and the role it is drawn in.
struct Segment {
    text: String,
    role: Role,
    /// Segments that give up their width first when the line does not fit.
    elastic: bool,
}

fn segments(status: &Status, theme: &Theme) -> Vec<Segment> {
    let fixed = |text: String, role: Role| Segment {
        text,
        role,
        elastic: false,
    };

    let mut out = vec![
        fixed(
            match status.position {
                // A dash rather than 0: there is no zeroth row, and "0/12" reads like a
                // count that has gone wrong.
                None => format!("-/{}", status.count),
                Some(position) => format!("{position}/{}", status.count),
            },
            Role::Normal,
        ),
        fixed(
            format!(
                "sort: {} {}",
                status.sort_column.header(),
                theme.sort_arrow(status.sort_order == Order::Asc)
            ),
            Role::Normal,
        ),
        fixed(
            format!(
                "drafts: {}",
                if status.show_drafts {
                    "shown"
                } else {
                    "hidden"
                }
            ),
            Role::Dim,
        ),
    ];

    // Unlike drafts, wide mode says nothing when it is off: its default state needs no
    // explanation, and a permanent "wide: off" would just be noise on every frame.
    if status.wide {
        out.push(fixed("wide".to_owned(), Role::Dim));
    }

    out.push(refresh_segment(status, theme));

    if status.auth_paused {
        out.push(Segment {
            text: format!("auth failed{}check token", theme.dash()),
            role: Role::Failure,
            elastic: false,
        });
    }

    if status.truncated {
        out.push(fixed("capped".to_owned(), Role::Dim));
    }
    if let Some(lost) = &status.degraded {
        out.push(Segment {
            text: format!("degraded: no {lost}"),
            role: Role::Warning,
            elastic: true,
        });
    }
    if !status.dropped.is_empty() {
        let names: Vec<&str> = status.dropped.iter().map(|c| c.header()).collect();
        out.push(Segment {
            text: format!("dropped: {}", names.join(", ")),
            role: Role::Dim,
            elastic: true,
        });
    }
    if let Some(query) = &status.search {
        out.push(Segment {
            text: format!("/{query}"),
            role: Role::Accent,
            elastic: true,
        });
    }
    if let Some(message) = &status.flash {
        out.push(Segment {
            text: message.clone(),
            role: Role::Accent,
            elastic: true,
        });
    }

    out.push(fixed("[?] help".to_owned(), Role::Dim));
    // `Refresh::Fresh { next_in_secs: None }` renders empty text: an empty segment would
    // still draw its separator, producing a doubled one.
    out.retain(|segment| !segment.text.is_empty());
    out
}

fn refresh_segment(status: &Status, theme: &Theme) -> Segment {
    let frames = theme.spinner_frames();
    match &status.refresh {
        Refresh::Waiting => Segment {
            text: "waiting".to_owned(),
            role: Role::Dim,
            elastic: false,
        },
        Refresh::Fetching => Segment {
            text: format!("{} refreshing", frames[status.spinner % frames.len()]),
            role: Role::Pending,
            elastic: false,
        },
        Refresh::Fresh { next_in_secs, .. } => Segment {
            text: match next_in_secs {
                Some(next) => format!("next refresh in {}", compact_seconds(*next)),
                None => String::new(),
            },
            role: Role::Success,
            elastic: true,
        },
        Refresh::Cached {
            age_secs,
            refreshing,
        } => Segment {
            text: if *refreshing {
                format!(
                    "cached {} ago{}refreshing",
                    compact_seconds(*age_secs),
                    theme.separator()
                )
            } else {
                format!("cached {} ago", compact_seconds(*age_secs))
            },
            role: Role::Warning,
            elastic: true,
        },
        Refresh::Stale { retry_in_secs } => Segment {
            text: match retry_in_secs {
                Some(retry) => format!(
                    "stale{}retrying in {}",
                    theme.dash(),
                    compact_seconds(*retry)
                ),
                None => "stale".to_owned(),
            },
            role: Role::Warning,
            elastic: false,
        },
        // The one segment that can be arbitrarily long: a server error message is
        // whatever the server felt like saying. It is truncated here and the full text
        // kept for the log popup.
        Refresh::Offline { error } => Segment {
            text: format!("offline{}{}", theme.dash(), theme.ascii_safe(error)),
            role: Role::Failure,
            elastic: true,
        },
    }
}

/// Build the status bar for one frame.
pub fn build<'a>(status: &Status, theme: &Theme, width: u16) -> Line<'a> {
    let width = usize::from(width);
    if width == 0 {
        return Line::default();
    }

    let separator = theme.separator();
    let mut segments = segments(status, theme);
    fit(&mut segments, separator.width(), width, theme);

    let mut spans: Vec<Span<'a>> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(separator.to_owned(), theme.style(Role::Dim)));
        }
        spans.push(Span::styled(
            segment.text.clone(),
            theme.style(segment.role),
        ));
    }

    Line::from(spans)
}

/// Shrink the line to `width`, taking it out of the elastic segments first.
///
/// Truncating the tail instead would drop `[?] help` and the refresh state — the two
/// things a narrow terminal most needs — to keep a long error message that is already
/// in the log popup.
fn fit(segments: &mut Vec<Segment>, separator: usize, width: usize, theme: &Theme) {
    let total = |segments: &[Segment]| -> usize {
        let content: usize = segments.iter().map(|s| s.text.width()).sum();
        content + separator * segments.len().saturating_sub(1)
    };

    // Elastic segments give up width, longest first, down to a stub that still says
    // which kind of thing was there.
    const STUB: usize = 8;
    while total(segments) > width {
        let longest = segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.elastic && s.text.width() > STUB)
            .max_by_key(|(_, s)| s.text.width())
            .map(|(index, _)| index);

        let Some(index) = longest else { break };
        let over = total(segments) - width;
        let target = segments[index].text.width().saturating_sub(over).max(STUB);
        segments[index].text =
            crate::ui::table::truncate(&segments[index].text, target, theme.ellipsis());
    }

    // Still too wide: drop whole segments from the right, keeping the position and sort
    // at the left, until what remains fits.
    while segments.len() > 1 && total(segments) > width {
        segments.pop();
    }
    if let Some(first) = segments.first_mut()
        && first.text.width() > width
    {
        first.text = crate::ui::table::truncate(&first.text, width, theme.ellipsis());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::Tabs;
    use crate::config::schema::{Filter, Scope, Sort};
    use crate::gitlab::model::fixtures::mr;
    use crate::gitlab::query::Fragment;
    use crate::term::caps::{Capabilities, ColorDepth, NotifyEscape};
    use std::time::Duration;

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

    fn status() -> Status {
        Status {
            position: Some(3),
            count: 12,
            sort_column: Column::Updated,
            sort_order: Order::Desc,
            show_drafts: false,
            wide: false,
            refresh: Refresh::Fresh {
                ago_secs: 90,
                next_in_secs: Some(210),
            },
            auth_paused: false,
            search: None,
            degraded: None,
            dropped: Vec::new(),
            truncated: false,
            flash: None,
            spinner: 0,
        }
    }

    fn rendered(status: &Status, width: u16) -> String {
        build(status, &theme(), width).to_string()
    }

    fn tab() -> Tab {
        let mut tabs = Tabs::new(
            &[Filter::named("One", Scope::Assigned)],
            Sort::default(),
            true,
            None,
        );
        tabs.get_mut(0).unwrap().clone()
    }

    /// The status bar's segments, in display order.
    #[test]
    fn the_line_carries_every_documented_segment() {
        let mut status = status();
        status.search = Some("dark".into());
        let line = rendered(&status, 200);

        assert!(line.contains("3/12"), "{line}");
        assert!(line.contains("sort: UPDATED"), "{line}");
        assert!(line.contains("drafts: hidden"), "{line}");
        assert!(line.contains("next refresh in 3m"), "{line}");
        assert!(line.contains("/dark"), "{line}");
        assert!(line.contains("[?] help"), "{line}");
    }

    #[test]
    fn the_sort_direction_is_shown() {
        let mut status = status();
        let descending = rendered(&status, 200);
        status.sort_order = Order::Asc;
        let ascending = rendered(&status, 200);

        assert_ne!(descending, ascending, "direction has to be visible");
        assert!(ascending.contains(theme().sort_arrow(true)), "{ascending}");
    }

    /// There is no zeroth row, and `0/12` reads like a broken count.
    #[test]
    fn an_unselected_list_shows_a_dash_not_a_zero() {
        let mut status = status();
        status.position = None;
        assert!(rendered(&status, 200).contains("-/12"));
    }

    /// Unlike drafts, wide mode says nothing when it is off — its default needs no
    /// explanation, and a permanent "wide: off" would just be noise.
    #[test]
    fn wide_mode_is_named_only_when_it_is_on() {
        let mut status = status();
        assert!(!rendered(&status, 200).contains("wide"));

        status.wide = true;
        assert!(rendered(&status, 200).contains("wide"));
    }

    #[test]
    fn each_refresh_state_has_its_documented_wording() {
        let mut status = status();

        status.refresh = Refresh::Fetching;
        assert!(rendered(&status, 200).contains("refreshing"));

        status.refresh = Refresh::Stale {
            retry_in_secs: Some(8),
        };
        assert!(rendered(&status, 200).contains("stale — retrying in 8s"));

        status.refresh = Refresh::Offline {
            error: "connection refused".into(),
        };
        assert!(rendered(&status, 200).contains("offline — connection refused"));
    }

    /// The auth-pause segment is orthogonal to `refresh`, not a replacement for it: a
    /// warm-started tab must keep saying its rows are hours old even while paused —
    /// that is exactly when the warning matters most.
    #[test]
    fn an_auth_pause_composes_with_the_refresh_state_instead_of_replacing_it() {
        let mut status = status();
        status.auth_paused = true;
        assert!(rendered(&status, 200).contains("auth failed — check token"));

        status.refresh = Refresh::Cached {
            age_secs: 10_800,
            refreshing: false,
        };
        let line = rendered(&status, 200);
        assert!(line.contains("cached 3h ago"), "{line}");
        assert!(line.contains("auth failed — check token"), "{line}");
    }

    #[test]
    fn the_spinner_advances_with_the_frame() {
        let mut status = status();
        status.refresh = Refresh::Fetching;

        let first = rendered(&status, 200);
        status.spinner = 1;
        let second = rendered(&status, 200);

        assert_ne!(first, second, "the spinner has to move");
    }

    /// A server error is whatever the server felt like saying; the full text lives in the
    /// log popup.
    #[test]
    fn a_long_error_is_truncated_to_the_width() {
        let mut status = status();
        status.refresh = Refresh::Offline {
            error: "x".repeat(400),
        };

        let line = rendered(&status, 80);
        assert!(line.width() <= 80, "`{line}` is {} cells", line.width());
        assert!(line.contains("offline"), "and still says what happened");
    }

    /// The two things a narrow terminal most needs are where it is and what is wrong, so
    /// the error gives up width before they are dropped.
    #[test]
    fn shrinking_takes_width_from_the_error_before_the_position() {
        let mut status = status();
        status.refresh = Refresh::Offline {
            error: "a very long explanation that will not fit anywhere near here".into(),
        };

        let line = rendered(&status, 60);
        assert!(line.contains("3/12"), "{line}");
        assert!(line.width() <= 60);
    }

    #[test]
    fn a_degraded_query_says_what_is_missing() {
        let mut status = status();
        status.degraded = Some("reviewers, labels".into());

        let line = rendered(&status, 200);
        assert!(line.contains("degraded: no reviewers, labels"), "{line}");
    }

    /// The status bar notes the columns the width forced out.
    #[test]
    fn dropped_columns_are_named() {
        let mut status = status();
        status.dropped = vec![Column::Age, Column::Diff];

        let line = rendered(&status, 200);
        assert!(line.contains("dropped: AGE, DIFF"), "{line}");
    }

    #[test]
    fn a_capped_list_says_so() {
        let mut status = status();
        status.truncated = true;
        assert!(rendered(&status, 200).contains("capped"));
    }

    #[test]
    fn a_flash_is_shown_when_present() {
        let mut status = status();
        status.flash = Some("no pipeline".into());
        assert!(rendered(&status, 200).contains("no pipeline"));
    }

    /// A dragged window edge passes through every width on the way.
    #[test]
    fn degenerate_widths_do_not_panic() {
        let mut status = status();
        status.refresh = Refresh::Offline {
            error: "something went wrong somewhere far away".into(),
        };
        status.flash = Some("copied URL".into());

        for width in 0..120 {
            let line = rendered(&status, width);
            assert!(
                line.width() <= usize::from(width),
                "width {width}: `{line}` is {} cells",
                line.width()
            );
        }
    }

    /// A failure over a previous snapshot is stale; a failure over nothing is offline.
    /// Saying "stale" above an empty table implies there is something there to distrust.
    #[test]
    fn a_failure_is_stale_only_when_there_is_something_to_show() {
        let now = Instant::now();
        let error = crate::error::Error::Other("boom".into());

        let mut fresh = tab();
        fresh.apply_error(&error);
        assert!(matches!(refresh_of(&fresh, now), Refresh::Offline { .. }));

        let mut loaded = tab();
        loaded.apply_rows(vec![mr("a", "x")], now);
        loaded.apply_error(&error);
        assert!(matches!(refresh_of(&loaded, now), Refresh::Stale { .. }));
    }

    /// Cached rows are painted immediately and *marked*. An unmarked warm
    /// start is the failure mode that matters — the user acts on hours-old data believing
    /// it is current.
    #[test]
    fn a_warm_started_tab_is_marked_as_cached() {
        let now = Instant::now();
        let mut tab = tab();
        tab.apply_cached(
            vec![mr("a", "x")],
            Duration::from_secs(3 * 3600),
            false,
            Fragment::full(),
        );

        assert_eq!(
            refresh_of(&tab, now),
            Refresh::Cached {
                age_secs: 3 * 3600,
                refreshing: false
            }
        );

        let mut status = status();
        status.refresh = refresh_of(&tab, now);
        let line = rendered(&status, 200);
        assert!(line.contains("cached"), "{line}");
        assert!(
            !line.contains("refreshed"),
            "not this session's data: {line}"
        );
    }

    /// Cache-warm marks the rows *while* the first live fetch runs, so the spinner must not
    /// replace the marker — a spinner alone implies the rows on screen are current.
    #[test]
    fn the_cached_marker_survives_the_first_live_fetch_starting() {
        let now = Instant::now();
        let mut tab = tab();
        tab.apply_cached(
            vec![mr("a", "x")],
            Duration::from_secs(60),
            false,
            Fragment::full(),
        );
        tab.begin_fetch();

        assert_eq!(
            refresh_of(&tab, now),
            Refresh::Cached {
                age_secs: 60,
                refreshing: true
            }
        );
        assert!(rendered_with(&tab, now).contains("refreshing"));
    }

    /// Once a live fetch lands the marker has to go, or the status bar keeps calling this
    /// session's own data cached.
    #[test]
    fn a_live_fetch_replaces_the_cached_marker_with_a_fresh_one() {
        let now = Instant::now();
        let mut tab = tab();
        tab.apply_cached(
            vec![mr("a", "x")],
            Duration::from_secs(60),
            false,
            Fragment::full(),
        );
        tab.apply_rows(vec![mr("a", "x")], now);

        assert!(matches!(refresh_of(&tab, now), Refresh::Fresh { .. }));
    }

    /// A first fetch that fails over cached rows is stale, not offline: there is a
    /// populated table on screen, and "offline" says there is nothing there.
    #[test]
    fn a_failure_over_cached_rows_is_stale_rather_than_offline() {
        let now = Instant::now();
        let mut tab = tab();
        tab.apply_cached(
            vec![mr("a", "x")],
            Duration::from_secs(60),
            false,
            Fragment::full(),
        );
        tab.apply_error(&crate::error::Error::Other("boom".into()));

        assert!(matches!(refresh_of(&tab, now), Refresh::Stale { .. }));
    }

    fn rendered_with(tab: &Tab, now: Instant) -> String {
        let mut status = status();
        status.refresh = refresh_of(tab, now);
        rendered(&status, 200)
    }

    #[test]
    fn the_countdown_comes_from_the_schedulers_deadline() {
        let now = Instant::now();
        let mut tab = tab();
        tab.apply_rows(Vec::new(), now);
        tab.next_refresh = Some(now + Duration::from_secs(120));

        match refresh_of(&tab, now) {
            Refresh::Fresh { next_in_secs, .. } => assert_eq!(next_in_secs, Some(120)),
            other => panic!("{other:?}"),
        }
    }

    /// A deadline that has already passed counts down to zero rather than going negative.
    #[test]
    fn an_overdue_refresh_does_not_count_backwards() {
        let now = Instant::now();
        let mut tab = tab();
        tab.apply_rows(Vec::new(), now);
        tab.next_refresh = Some(now - Duration::from_secs(30));

        match refresh_of(&tab, now) {
            Refresh::Fresh { next_in_secs, .. } => assert_eq!(next_in_secs, Some(0)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_tab_that_has_never_fetched_is_waiting() {
        assert_eq!(refresh_of(&tab(), Instant::now()), Refresh::Waiting);
    }

    /// The ascii theme exists for terminals that render nothing else. A `·` or an em dash
    /// formatted into a message rather than taken from the theme is invisible in review
    /// and only shows up on the terminal that cannot draw it.
    #[test]
    fn the_ascii_theme_emits_no_non_ascii() {
        let caps = Capabilities {
            color: ColorDepth::None,
            hyperlinks: false,
            notify: NotifyEscape::None,
            focus_events: true,
            multiplexed: false,
            over_ssh: true,
        };
        let ascii = Theme::builtin("catppuccin-mocha", true, &caps);

        let states = [
            Refresh::Waiting,
            Refresh::Fetching,
            Refresh::Fresh {
                ago_secs: 90,
                next_in_secs: Some(210),
            },
            Refresh::Cached {
                age_secs: 10_800,
                refreshing: false,
            },
            Refresh::Stale {
                retry_in_secs: Some(8),
            },
            Refresh::Offline {
                error: "instance rejected the token — check its scope".into(),
            },
        ];

        for refresh in states {
            let mut status = status();
            status.refresh = refresh;
            status.auth_paused = true;
            status.search = Some("q".into());
            status.degraded = Some("labels".into());
            status.dropped = vec![Column::Age];
            status.truncated = true;
            status.flash = Some("copied URL".into());
            status.wide = true;

            let line = build(&status, &ascii, 400).to_string();
            assert!(line.is_ascii(), "non-ascii in the ascii theme: `{line}`");
        }
    }
}
