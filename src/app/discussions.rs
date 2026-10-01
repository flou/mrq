//! The comments popup's data: what it is showing, what it remembers, how it reads.
//!
//! The popup opens before its data exists, so it has three states rather than one list.
//! Results are remembered per merge request for as long as GitLab says nothing new
//! happened to it (`updated_at` is bumped by every comment, resolve and edit), which makes
//! reopening the popup instant without ever showing a stale thread.

use std::collections::HashMap;

use jiff::Timestamp;

use crate::app::event::{AppEvent, EventSender};
use crate::gitlab::client::Client;
use crate::gitlab::discussions::{Discussion, Note};
use crate::gitlab::model::MergeRequest;
use crate::ui::markdown::{self, Segment, StyledLine};
use crate::ui::theme::{Role, Theme};

/// What the comments popup is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscussionsView {
    Loading,
    Loaded(Vec<Discussion>),
    Failed(String),
}

/// Fetched threads, keyed by the merge request's global id.
///
/// In memory only: a few KiB of text per merge request that the next run refetches in one
/// request is not worth a cache-format version.
#[derive(Debug, Default)]
pub struct DiscussionsCache {
    entries: HashMap<String, (Timestamp, Vec<Discussion>)>,
}

impl DiscussionsCache {
    /// The remembered threads, if they were fetched at the merge request's current
    /// `updated_at`.
    pub fn get(&self, mr: &MergeRequest) -> Option<&Vec<Discussion>> {
        self.entries
            .get(&mr.id)
            .filter(|(updated_at, _)| *updated_at == mr.updated_at)
            .map(|(_, discussions)| discussions)
    }

    pub fn insert(&mut self, id: String, updated_at: Timestamp, discussions: Vec<Discussion>) {
        self.entries.insert(id, (updated_at, discussions));
    }
}

/// Fetch one merge request's threads in the background and publish the outcome.
///
/// Deliberately not tracked in `Tasks`: it holds nothing the terminal guard needs
/// released, writes nothing to the screen, and is a single request that dies with the
/// runtime if the user quits while it is in flight.
pub fn spawn(
    events: EventSender,
    client: Client,
    id: String,
    updated_at: Timestamp,
    project_path: String,
    iid: String,
) {
    tokio::spawn(async move {
        let result = crate::gitlab::discussions::fetch(&client, &project_path, &iid)
            .await
            .map_err(|error| error.to_string());
        let _ = events.send(AppEvent::DiscussionsLoaded {
            id,
            updated_at,
            result: Box::new(result),
        });
    });
}

/// The popup's text, wrapped to `width` cells.
pub fn lines(
    mr: &MergeRequest,
    view: &DiscussionsView,
    theme: &Theme,
    width: usize,
) -> Vec<StyledLine> {
    let plain = |role: Role, text: String| vec![Segment { role, text }];

    let mut out = vec![
        plain(Role::Normal, format!("{} !{}", mr.project_name, mr.iid)),
        plain(Role::Normal, mr.title.clone()),
        Vec::new(),
    ];

    match view {
        DiscussionsView::Loading => out.push(plain(Role::Pending, "Loading discussions...".into())),
        DiscussionsView::Failed(message) => {
            out.push(plain(
                Role::Failure,
                format!("Could not load discussions: {message}"),
            ));
            out.push(plain(
                Role::Dim,
                "Close the popup and reopen it to retry.".into(),
            ));
        }
        DiscussionsView::Loaded(discussions) if discussions.is_empty() => {
            out.push(plain(Role::Dim, "No comments.".into()));
        }
        DiscussionsView::Loaded(discussions) => {
            for (index, discussion) in discussions.iter().enumerate() {
                if index > 0 {
                    out.push(Vec::new());
                }
                thread(&mut out, discussion, theme, width);
            }
        }
    }

    out.into_iter()
        .map(|line| {
            line.into_iter()
                .map(|segment| Segment {
                    role: segment.role,
                    text: theme.ascii_safe(&segment.text).into_owned(),
                })
                .collect()
        })
        .collect()
}

/// One thread: a status line, then each note under it.
fn thread(out: &mut Vec<StyledLine>, discussion: &Discussion, theme: &Theme, width: usize) {
    let (role, status) = if discussion.is_unresolved() {
        (Role::Warning, "[unresolved]")
    } else if discussion.resolvable {
        (Role::Success, "[resolved]")
    } else {
        (Role::Dim, "[comment]")
    };

    let mut header = vec![Segment {
        role,
        text: status.to_owned(),
    }];
    // A diff comment says where; the first note carries it for the whole thread.
    if let Some(position) = discussion.notes.first().and_then(|n| n.position.as_ref()) {
        let place = position.line.map_or_else(
            || position.path.clone(),
            |line| format!("{}:{line}", position.path),
        );
        header.push(Segment {
            role: Role::Accent,
            text: format!(" {place}"),
        });
    }
    out.push(header);

    for (index, note) in discussion.notes.iter().enumerate() {
        // Replies sit one level in from the comment that opened the thread.
        let indent = if index == 0 { 2 } else { 4 };
        note_lines(out, note, indent, theme, width);
    }
}

fn note_lines(out: &mut Vec<StyledLine>, note: &Note, indent: usize, theme: &Theme, width: usize) {
    let pad = " ".repeat(indent);

    let mut byline = format!("{pad}@{}", note.author.username);
    if let Some(at) = note.created_at {
        byline.push_str(theme.dash());
        byline.push_str(&at.strftime("%Y-%m-%d %H:%M").to_string());
    }
    out.push(vec![Segment {
        role: Role::Header,
        text: byline,
    }]);

    let body_width = if width == 0 {
        0
    } else {
        width.saturating_sub(indent).max(1)
    };
    for mut line in markdown::render(&note.body, body_width) {
        line.insert(
            0,
            Segment {
                role: Role::Normal,
                text: pad.clone(),
            },
        );
        out.push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitlab::discussions::Position;
    use crate::gitlab::model::User;
    use crate::gitlab::model::fixtures::mr;
    use crate::term::caps::{Capabilities, ColorDepth, NotifyEscape};

    fn theme(ascii: bool) -> Theme {
        let caps = Capabilities {
            color: ColorDepth::TrueColor,
            hyperlinks: false,
            notify: NotifyEscape::None,
            focus_events: true,
            multiplexed: false,
            over_ssh: false,
        };
        Theme::builtin("catppuccin-mocha", ascii, &caps)
    }

    fn note(author: &str, body: &str) -> Note {
        Note {
            author: User::new(author),
            body: body.to_owned(),
            created_at: Some("2026-09-11T08:30:00Z".parse().unwrap()),
            position: None,
        }
    }

    fn text(lines: &[StyledLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn the_cache_only_answers_for_the_updated_at_it_was_filled_at() {
        let mut m = mr("a", "someone");
        let mut cache = DiscussionsCache::default();
        assert!(cache.get(&m).is_none());

        cache.insert(m.id.clone(), m.updated_at, Vec::new());
        assert!(cache.get(&m).is_some());

        m.updated_at = "2026-09-12T00:00:00Z".parse().unwrap();
        assert!(cache.get(&m).is_none(), "a newer merge request is a miss");
    }

    #[test]
    fn loading_and_failure_say_so() {
        let m = mr("a", "someone");

        let loading = text(&lines(&m, &DiscussionsView::Loading, &theme(false), 60));
        assert!(loading.iter().any(|l| l.contains("Loading")), "{loading:?}");

        let failed = text(&lines(
            &m,
            &DiscussionsView::Failed("HTTP 500".into()),
            &theme(false),
            60,
        ));
        assert!(failed.iter().any(|l| l.contains("HTTP 500")), "{failed:?}");

        let empty = text(&lines(
            &m,
            &DiscussionsView::Loaded(Vec::new()),
            &theme(false),
            60,
        ));
        assert!(empty.iter().any(|l| l == "No comments."), "{empty:?}");
    }

    #[test]
    fn threads_show_state_place_author_and_replies() {
        let m = mr("a", "someone");
        let mut opener = note("jdoe", "Please rename this");
        opener.position = Some(Position {
            path: "src/lib.rs".into(),
            line: Some(42),
        });
        let view = DiscussionsView::Loaded(vec![
            Discussion {
                resolvable: true,
                resolved: false,
                notes: vec![opener, note("bwayne", "Done")],
            },
            Discussion {
                resolvable: true,
                resolved: true,
                notes: vec![note("jdoe", "Fine")],
            },
        ]);

        let rendered = text(&lines(&m, &view, &theme(false), 60));

        assert!(
            rendered.contains(&"[unresolved] src/lib.rs:42".to_owned()),
            "{rendered:?}"
        );
        assert!(rendered.contains(&"[resolved]".to_owned()), "{rendered:?}");
        assert!(
            rendered.contains(&"  @jdoe — 2026-09-11 08:30".to_owned()),
            "{rendered:?}"
        );
        assert!(
            rendered.contains(&"    @bwayne — 2026-09-11 08:30".to_owned()),
            "{rendered:?}"
        );
        assert!(
            rendered.contains(&"  Please rename this".to_owned()),
            "{rendered:?}"
        );
    }

    #[test]
    fn the_ascii_theme_leaves_nothing_non_ascii() {
        let m = mr("a", "someone");
        let view = DiscussionsView::Loaded(vec![Discussion {
            resolvable: false,
            resolved: false,
            notes: vec![note("jdoe", "Nice 👍")],
        }]);

        for line in text(&lines(&m, &view, &theme(true), 60)) {
            assert!(line.is_ascii(), "{line:?}");
        }
    }
}
