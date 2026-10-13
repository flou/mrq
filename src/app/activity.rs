//! The activity feed: a persisted history of what changed across every filter.
//!
//! Notifications are an interruption budget — `notify` spends it carefully, and anything
//! it decides not to spend is gone. The feed is the record those decisions are made from:
//! every [`diff::Event`](crate::app::diff::Event), whatever the `on_*` toggles say, kept
//! so the user can look back at what happened while they were away.
//!
//! # Moves are pairs
//!
//! A merge request moving from one filter to another is two diffs of two filters on two
//! independent timers: it leaves one, and it arrives in the other, in either order and up
//! to a refresh interval apart. [`Activity::record`] pairs them up within
//! [`CORRELATE_WINDOW`] into one [`EntryKind::Moved`], which is the fact the user cares
//! about.
//!
//! # One change, one entry
//!
//! The same approval seen through two filters is one approval. An entry identical to one
//! recorded within the window is dropped, for the same reason the notification dedup key
//! excludes the filter.
//!
//! # The file is bounded
//!
//! Like the notification dedup store, it is pruned on every write — by age, then by
//! count — because it is read on the startup critical path.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::app::diff::{Event, Trigger};
use crate::gitlab::model::{MrState, PipelineStatus};

/// How far apart a departure and an arrival can be and still read as one move, and how
/// close two identical entries have to be to count as the same change.
///
/// Comfortably above the default refresh interval: the two halves of a move come from
/// different filters, each on its own timer.
const CORRELATE_WINDOW: SignedDuration = SignedDuration::from_mins(15);

/// How long an entry is kept. A feed is for "what happened while I was away", not an
/// audit log.
const MAX_AGE: SignedDuration = SignedDuration::from_hours(24 * 7);

/// The hard cap, applied after the age prune.
const MAX_ENTRIES: usize = 500;

const VERSION: u32 = 1;

/// What happened to the merge request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EntryKind {
    /// Appeared in the filter.
    New,
    /// Disappeared from the filter: merged, closed, or no longer matching it.
    Left,
    /// Left `from` and appeared in the entry's filter.
    Moved {
        from: String,
    },
    Approval {
        by: Vec<String>,
    },
    Discussion {
        added: u32,
    },
    Pipeline {
        from: Option<PipelineStatus>,
        to: Option<PipelineStatus>,
    },
    MergedOrClosed {
        state: MrState,
    },
}

impl EntryKind {
    fn from_trigger(trigger: &Trigger) -> Self {
        match trigger {
            Trigger::NewMergeRequest => Self::New,
            Trigger::LeftFilter => Self::Left,
            Trigger::Approval { by } => Self::Approval { by: by.clone() },
            Trigger::Discussion { added } => Self::Discussion { added: *added },
            Trigger::Pipeline { from, to } => Self::Pipeline {
                from: from.clone(),
                to: to.clone(),
            },
            Trigger::MergedOrClosed { state } => Self::MergedOrClosed { state: *state },
        }
    }

    /// A short label for the feed's kind column.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Left => "left",
            Self::Moved { .. } => "moved",
            Self::Approval { .. } => "approved",
            Self::Discussion { .. } => "discussion",
            Self::Pipeline { .. } => "pipeline",
            Self::MergedOrClosed {
                state: MrState::Merged,
            } => "merged",
            Self::MergedOrClosed { .. } => "closed",
        }
    }
}

/// One line of the feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub at: Timestamp,
    /// The filter, by name rather than index: indices shift when the config changes
    /// between sessions, and a persisted entry must not start pointing at another tab.
    /// For a move, the destination.
    pub filter_name: String,
    pub kind: EntryKind,
    pub mr_id: String,
    pub iid: String,
    pub project_name: String,
    pub title: String,
}

impl Entry {
    fn from_event(event: &Event, at: Timestamp) -> Self {
        Self {
            at,
            filter_name: event.filter_name.clone(),
            kind: EntryKind::from_trigger(&event.trigger),
            mr_id: event.id.clone(),
            iid: event.iid.clone(),
            project_name: event.project_name.clone(),
            title: event.title.clone(),
        }
    }

    /// What happened, as prose, without the merge request's title.
    pub fn summary(&self) -> String {
        match &self.kind {
            EntryKind::New => format!("new in {}", self.filter_name),
            EntryKind::Left => format!("left {}", self.filter_name),
            EntryKind::Moved { from } => format!("moved {from} → {}", self.filter_name),
            EntryKind::Approval { by } => format!("approved by {}", by.join(", ")),
            EntryKind::Discussion { added } => {
                let plural = if *added == 1 {
                    "discussion"
                } else {
                    "discussions"
                };
                format!("{added} new {plural}")
            }
            EntryKind::Pipeline { to: Some(to), .. } => format!("pipeline {}", to.label()),
            EntryKind::Pipeline { to: None, .. } => "pipeline removed".to_owned(),
            EntryKind::MergedOrClosed { state } => state.label().to_owned(),
        }
    }
}

/// The persisted form.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Stored {
    version: u32,
    last_viewed: Option<Timestamp>,
    entries: VecDeque<Entry>,
}

/// The feed, oldest entry first, backed by a file under `$XDG_STATE_HOME/mrq/`.
#[derive(Debug, Clone)]
pub struct Activity {
    entries: VecDeque<Entry>,
    /// When the feed was last opened, which is what "unread" is counted from.
    last_viewed: Option<Timestamp>,
    path: Option<PathBuf>,
}

impl Activity {
    /// A feed with no file behind it, for a session whose state directory could not be
    /// created, and for tests.
    pub const fn ephemeral() -> Self {
        Self {
            entries: VecDeque::new(),
            last_viewed: None,
            path: None,
        }
    }

    /// Load from the state directory, pruning as it goes.
    ///
    /// Unreadable, corrupt or from another format version is treated as empty, never
    /// fatal: losing the history is a far smaller cost than refusing to start.
    pub fn load(state_dir: &Path, now: Timestamp) -> Self {
        let path = state_dir.join("activity.json");

        let stored = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
                Ok(stored) if stored.version == VERSION => stored,
                Ok(stored) => {
                    tracing::info!(
                        version = stored.version,
                        "discarding activity feed from another format version"
                    );
                    Stored::default()
                }
                Err(error) => {
                    tracing::info!(%error, "activity feed is unreadable; starting empty");
                    Stored::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(error) => {
                tracing::debug!(%error, "could not read the activity feed");
                Stored::default()
            }
        };

        let mut activity = Self {
            entries: stored.entries,
            last_viewed: stored.last_viewed,
            path: Some(path),
        };
        activity.prune(now);
        activity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every entry, newest first, which is the order the feed is read in.
    pub fn newest_first(&self) -> Vec<Entry> {
        self.entries.iter().rev().cloned().collect()
    }

    /// How many entries arrived since the feed was last opened.
    pub fn unread(&self) -> usize {
        match self.last_viewed {
            None => self.entries.len(),
            Some(seen) => self
                .entries
                .iter()
                .rev()
                .take_while(|entry| entry.at > seen)
                .count(),
        }
    }

    /// The user opened the feed.
    pub fn mark_read(&mut self, now: Timestamp) {
        if self.unread() == 0 {
            return;
        }
        self.last_viewed = Some(now);
        self.save();
    }

    /// Add one refresh's events, pairing departures with arrivals into moves and dropping
    /// repeats of a change already recorded through another filter.
    ///
    /// A pipeline moving into an in-progress status (created, pending, running...) is not
    /// recorded: a single run would otherwise fill the feed before reaching its outcome.
    pub fn record(&mut self, events: &[Event], now: Timestamp) {
        let mut events = events
            .iter()
            .filter(|e| !is_in_progress(&e.trigger))
            .peekable();
        if events.peek().is_none() {
            return;
        }
        for event in events {
            self.push(Entry::from_event(event, now));
        }
        self.prune(now);
        self.save();
    }

    fn push(&mut self, entry: Entry) {
        let window_start = entry.at - CORRELATE_WINDOW;
        let recent = |e: &&mut Entry| e.at >= window_start && e.mr_id == entry.mr_id;

        match &entry.kind {
            // Arrived here after leaving somewhere else: the departure becomes the move.
            EntryKind::New => {
                if let Some(left) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .filter(recent)
                    .find(|e| e.kind == EntryKind::Left && e.filter_name != entry.filter_name)
                {
                    left.kind = EntryKind::Moved {
                        from: std::mem::take(&mut left.filter_name),
                    };
                    left.filter_name = entry.filter_name;
                    left.at = entry.at;
                    return;
                }
            }
            // Left here after arriving somewhere else: the arrival becomes the move.
            EntryKind::Left => {
                if let Some(new) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .filter(recent)
                    .find(|e| e.kind == EntryKind::New && e.filter_name != entry.filter_name)
                {
                    new.kind = EntryKind::Moved {
                        from: entry.filter_name,
                    };
                    new.at = entry.at;
                    return;
                }
            }
            // The same change seen through a second filter. Only another filter's entry
            // counts: the same filter reporting it again is a new change — a second
            // comment, or a retried pipeline failing again.
            _ => {
                if self
                    .entries
                    .iter()
                    .rev()
                    .take_while(|e| e.at >= window_start)
                    .any(|e| {
                        e.mr_id == entry.mr_id
                            && e.kind == entry.kind
                            && e.filter_name != entry.filter_name
                    })
                {
                    return;
                }
            }
        }
        self.entries.push_back(entry);
    }

    /// Drop what is too old, then the oldest past the cap.
    fn prune(&mut self, now: Timestamp) {
        self.entries.retain(|e| now.duration_since(e.at) <= MAX_AGE);
        // Moves rewrite an older entry's timestamp in place, so the deque is only roughly
        // ordered; keep it sorted so "oldest" and "newest" mean what they say.
        self.entries.make_contiguous().sort_by_key(|e| e.at);
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    /// Persist. A failure costs the history after a restart, so it is logged and
    /// otherwise ignored.
    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let stored = Stored {
            version: VERSION,
            last_viewed: self.last_viewed,
            entries: self.entries.clone(),
        };
        let json = match serde_json::to_vec(&stored) {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(%error, "could not serialise the activity feed");
                return;
            }
        };
        if let Err(error) = crate::app::notify::write_atomic(path, &json) {
            tracing::warn!(path = %path.display(), %error, "could not save the activity feed");
        }
    }
}

const fn is_in_progress(trigger: &Trigger) -> bool {
    matches!(trigger, Trigger::Pipeline { to: Some(to), .. } if to.is_active())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap() + SignedDuration::from_mins(minutes)
    }

    fn event(filter: &str, id: &str, trigger: Trigger) -> Event {
        Event {
            trigger,
            filter: 0,
            filter_name: filter.to_owned(),
            id: id.to_owned(),
            iid: "7".to_owned(),
            project_name: "web".to_owned(),
            title: "Fix it".to_owned(),
        }
    }

    fn kinds(activity: &Activity) -> Vec<(String, EntryKind)> {
        activity
            .entries
            .iter()
            .map(|e| (e.filter_name.clone(), e.kind.clone()))
            .collect()
    }

    #[test]
    fn leaving_then_arriving_elsewhere_is_one_move() {
        let mut feed = Activity::ephemeral();
        feed.record(&[event("A", "x", Trigger::LeftFilter)], at(0));
        feed.record(&[event("B", "x", Trigger::NewMergeRequest)], at(3));

        assert_eq!(
            kinds(&feed),
            vec![(
                "B".to_owned(),
                EntryKind::Moved {
                    from: "A".to_owned()
                }
            )]
        );
        assert_eq!(feed.entries[0].at, at(3));
    }

    #[test]
    fn arriving_then_leaving_elsewhere_is_one_move() {
        let mut feed = Activity::ephemeral();
        feed.record(&[event("B", "x", Trigger::NewMergeRequest)], at(0));
        feed.record(&[event("A", "x", Trigger::LeftFilter)], at(3));

        assert_eq!(
            kinds(&feed),
            vec![(
                "B".to_owned(),
                EntryKind::Moved {
                    from: "A".to_owned()
                }
            )]
        );
    }

    #[test]
    fn a_departure_and_an_arrival_far_apart_stay_separate() {
        let mut feed = Activity::ephemeral();
        feed.record(&[event("A", "x", Trigger::LeftFilter)], at(0));
        feed.record(&[event("B", "x", Trigger::NewMergeRequest)], at(60));

        assert_eq!(
            kinds(&feed),
            vec![
                ("A".to_owned(), EntryKind::Left),
                ("B".to_owned(), EntryKind::New)
            ]
        );
    }

    #[test]
    fn leaving_and_returning_to_the_same_filter_is_not_a_move() {
        let mut feed = Activity::ephemeral();
        feed.record(&[event("A", "x", Trigger::LeftFilter)], at(0));
        feed.record(&[event("A", "x", Trigger::NewMergeRequest)], at(3));

        assert_eq!(
            kinds(&feed),
            vec![
                ("A".to_owned(), EntryKind::Left),
                ("A".to_owned(), EntryKind::New)
            ]
        );
    }

    #[test]
    fn the_same_change_seen_through_two_filters_is_recorded_once() {
        let mut feed = Activity::ephemeral();
        let approval = || Trigger::Approval {
            by: vec!["alice".to_owned()],
        };
        feed.record(&[event("A", "x", approval())], at(0));
        feed.record(&[event("B", "x", approval())], at(2));
        assert_eq!(feed.len(), 1);

        // A different approver is a different change.
        feed.record(
            &[event(
                "B",
                "x",
                Trigger::Approval {
                    by: vec!["bob".to_owned()],
                },
            )],
            at(3),
        );
        assert_eq!(feed.len(), 2);
    }

    #[test]
    fn the_same_change_repeated_in_one_filter_is_recorded_each_time() {
        let mut feed = Activity::ephemeral();
        let failed = || Trigger::Pipeline {
            from: Some(PipelineStatus::Running),
            to: Some(PipelineStatus::Failed),
        };
        let comment = || Trigger::Discussion { added: 1 };
        feed.record(&[event("A", "x", failed())], at(0));
        feed.record(&[event("A", "x", comment())], at(1));
        // A retry that fails again, and a second comment.
        feed.record(&[event("A", "x", failed())], at(5));
        feed.record(&[event("A", "x", comment())], at(6));
        assert_eq!(feed.len(), 4);
    }

    #[test]
    fn unread_counts_what_arrived_since_the_feed_was_opened() {
        let mut feed = Activity::ephemeral();
        feed.record(&[event("A", "x", Trigger::NewMergeRequest)], at(0));
        feed.record(&[event("A", "y", Trigger::NewMergeRequest)], at(1));
        assert_eq!(feed.unread(), 2);

        feed.mark_read(at(2));
        assert_eq!(feed.unread(), 0);

        feed.record(&[event("A", "z", Trigger::NewMergeRequest)], at(3));
        assert_eq!(feed.unread(), 1);
    }

    #[test]
    fn old_entries_and_the_excess_are_pruned() {
        let mut feed = Activity::ephemeral();
        feed.record(&[event("A", "old", Trigger::NewMergeRequest)], at(0));
        let events: Vec<Event> = (0..MAX_ENTRIES + 10)
            .map(|i| event("A", &format!("id-{i}"), Trigger::NewMergeRequest))
            .collect();
        feed.record(&events, at(60 * 24 * 8));

        assert_eq!(feed.len(), MAX_ENTRIES);
        assert!(feed.entries.iter().all(|e| e.mr_id != "old"));
    }

    #[test]
    fn the_feed_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut feed = Activity::load(dir.path(), at(0));
        feed.record(&[event("A", "x", Trigger::NewMergeRequest)], at(0));
        feed.record(
            &[event(
                "A",
                "x",
                Trigger::Pipeline {
                    from: Some(PipelineStatus::Running),
                    to: Some(PipelineStatus::Failed),
                },
            )],
            at(1),
        );
        feed.mark_read(at(2));

        let reloaded = Activity::load(dir.path(), at(3));
        assert_eq!(reloaded.newest_first(), feed.newest_first());
        assert_eq!(reloaded.unread(), 0);
    }

    #[test]
    fn in_progress_pipeline_statuses_are_not_recorded() {
        let pipeline = |from, to| Trigger::Pipeline { from, to };
        let mut feed = Activity::ephemeral();
        for to in [
            PipelineStatus::Created,
            PipelineStatus::WaitingForResource,
            PipelineStatus::Preparing,
            PipelineStatus::Pending,
            PipelineStatus::Running,
        ] {
            feed.record(&[event("A", "x", pipeline(None, Some(to)))], at(0));
        }
        assert!(feed.entries.is_empty());

        feed.record(
            &[
                event(
                    "A",
                    "x",
                    pipeline(Some(PipelineStatus::Running), Some(PipelineStatus::Failed)),
                ),
                event("A", "y", pipeline(Some(PipelineStatus::Running), None)),
            ],
            at(1),
        );
        assert_eq!(feed.len(), 2);
    }

    #[test]
    fn a_corrupt_file_is_an_empty_feed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("activity.json"), b"{not json").unwrap();
        assert_eq!(Activity::load(dir.path(), at(0)).len(), 0);
    }

    #[test]
    fn summaries_read_as_prose() {
        let entry = |kind| Entry {
            at: at(0),
            filter_name: "Review".to_owned(),
            kind,
            mr_id: "x".to_owned(),
            iid: "7".to_owned(),
            project_name: "web".to_owned(),
            title: "Fix it".to_owned(),
        };
        assert_eq!(
            entry(EntryKind::Moved {
                from: "Mine".to_owned()
            })
            .summary(),
            "moved Mine → Review"
        );
        assert_eq!(entry(EntryKind::Left).summary(), "left Review");
        assert_eq!(
            entry(EntryKind::MergedOrClosed {
                state: MrState::Merged
            })
            .summary(),
            "merged"
        );
    }
}
