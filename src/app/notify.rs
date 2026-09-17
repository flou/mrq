//! Turning diff triggers into the notifications actually delivered.
//!
//! `diff` decides what *changed*; this decides what is worth interrupting
//! someone for. Three filters sit between the two, and each exists because of a specific
//! way notifications become useless:
//!
//! - **Dedup** — a per-event key (`id` + kind) persisted in the state directory, so the
//!   same change is announced once even across a restart or when two filters both match
//!   the merge request.
//! - **Coalescing** — more than three events from one refresh collapse into one summary.
//!   A morning's backlog landing at once should be a line, not fifteen popups.
//! - **Focus gating** — `only_when_unfocused` suppresses while the user is already
//!   looking at the table, because a notification about something on screen is noise.
//!
//! # The dedup store is bounded
//!
//! It is a file that only ever gains entries, so it is pruned on every write: anything
//! past [`MAX_AGE`] goes, and if that still leaves more than [`MAX_ENTRIES`] the oldest
//! are dropped. Unbounded, a busy instance would grow it forever, and the file is read at
//! startup on the critical path.
//!
//! # Suppressed is still seen
//!
//! An event suppressed by focus gating is recorded in the dedup store anyway. The user was
//! looking at the table when it happened, so they have seen it; announcing it later when
//! they alt-tab away would be worse than not announcing it at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::app::diff::{Event, Trigger};
use crate::app::event::Notification;

/// "more than three events in one cycle collapse into a single … message".
const COALESCE_ABOVE: usize = 3;

/// How long a dedup key is remembered. Long enough that a daily user never sees a repeat,
/// short enough that the file cannot grow without bound.
const MAX_AGE: SignedDuration = SignedDuration::from_hours(24 * 30);

/// The hard cap, applied after the age prune. A busy instance can produce more keys in a
/// month than are worth keeping, and the file is read on the startup critical path.
const MAX_ENTRIES: usize = 4096;

const VERSION: u32 = 1;

/// Merge-request titles are arbitrarily long; a notification body is not.
const MAX_TITLE: usize = 60;

// ------------------------------------------------------------------------ focus gating

/// Whether notifications are delivered at all right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    pub only_when_unfocused: bool,
    /// Whether the terminal reports focus at all.
    pub focus_events: bool,
}

impl Gate {
    pub const fn allows(self, focused: bool) -> bool {
        if !self.only_when_unfocused {
            return true;
        }
        // With no focus events the setting is *ignored*, not applied
        // pessimistically. Treating an undetected terminal as permanently focused would
        // silently suppress everything the user asked for, which is the one outcome
        // worse than notifying too often. The startup log says so.
        if !self.focus_events {
            return true;
        }
        !focused
    }
}

// --------------------------------------------------------------------------- dedup store

/// The persisted set of already-announced events.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Stored {
    version: u32,
    /// Key to when it was first announced, which is what the prune orders by.
    seen: BTreeMap<String, Timestamp>,
}

/// The dedup store, backed by a file under `$XDG_STATE_HOME/mrq/`.
#[derive(Debug, Clone)]
pub struct Seen {
    seen: BTreeMap<String, Timestamp>,
    path: Option<PathBuf>,
}

impl Seen {
    /// An in-memory store with no file behind it, for a session whose state directory
    /// could not be created. Dedup within the session still works.
    pub const fn ephemeral() -> Self {
        Self {
            seen: BTreeMap::new(),
            path: None,
        }
    }

    /// Load from the state directory, pruning as it goes.
    ///
    /// Unreadable or corrupt is treated as empty, never fatal: the cost is one repeated
    /// notification, and refusing to start over a dedup file would be absurd.
    pub fn load(state_dir: &Path, now: Timestamp) -> Self {
        let path = state_dir.join("notified.json");

        let mut seen = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
                Ok(stored) if stored.version == VERSION => stored.seen,
                Ok(stored) => {
                    tracing::info!(
                        version = stored.version,
                        "discarding notification dedup store from another format version"
                    );
                    BTreeMap::new()
                }
                Err(error) => {
                    tracing::info!(%error, "notification dedup store is unreadable; starting empty");
                    BTreeMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                tracing::debug!(%error, "could not read the notification dedup store");
                BTreeMap::new()
            }
        };

        prune(&mut seen, now);
        Self {
            seen,
            path: Some(path),
        }
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    fn contains(&self, key: &str) -> bool {
        self.seen.contains_key(key)
    }

    fn record(&mut self, key: String, now: Timestamp) {
        self.seen.entry(key).or_insert(now);
    }

    /// Persist, pruning first so the file cannot grow without bound.
    ///
    /// A write failure costs a repeated notification after a restart, so it is logged and
    /// otherwise ignored.
    pub fn save(&mut self, now: Timestamp) {
        let Some(path) = self.path.clone() else {
            return;
        };
        prune(&mut self.seen, now);

        let stored = Stored {
            version: VERSION,
            seen: self.seen.clone(),
        };
        let json = match serde_json::to_vec(&stored) {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(%error, "could not serialise the notification dedup store");
                return;
            }
        };

        // Temp-and-rename so a crash mid-write cannot leave a corrupt file that the next
        // start has to throw away. The pid makes the name unique per process: two `mrq`
        // instances saving at once must not share a temp file, or one's `write` can land
        // between another's write and its rename, corrupting both — the rename itself is
        // atomic, but the content underneath a shared name would not be.
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        let result = std::fs::write(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(error) = &result {
            tracing::warn!(path = %path.display(), %error, "could not save the notification dedup store");
        }
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Drop what is too old, then what is over the cap, oldest first.
fn prune(seen: &mut BTreeMap<String, Timestamp>, now: Timestamp) {
    seen.retain(|_, at| now.duration_since(*at) <= MAX_AGE);

    if seen.len() <= MAX_ENTRIES {
        return;
    }
    // Oldest first, and drop exactly the excess.
    let mut by_age: Vec<(Timestamp, String)> =
        seen.iter().map(|(k, at)| (*at, k.clone())).collect();
    by_age.sort_unstable();

    for (_, key) in by_age.into_iter().take(seen.len() - MAX_ENTRIES) {
        seen.remove(&key);
    }
}

// ------------------------------------------------------------------------ the pipeline

/// Diff triggers in, notifications out.
#[derive(Debug)]
pub struct Notifier {
    gate: Gate,
    seen: Seen,
}

impl Notifier {
    pub const fn new(gate: Gate, seen: Seen) -> Self {
        Self { gate, seen }
    }

    /// Whether the terminal reports focus, for the startup log line.
    pub const fn ignores_focus_setting(&self) -> bool {
        self.gate.only_when_unfocused && !self.gate.focus_events
    }

    pub fn seen_count(&self) -> usize {
        self.seen.len()
    }

    /// Process one filter's refresh.
    ///
    /// `events` must all come from the same filter, which is how `diff` produces them:
    /// coalescing is per refresh cycle, and each filter refreshes on its own timer.
    pub fn process(
        &mut self,
        events: &[Event],
        focused: bool,
        now: Timestamp,
    ) -> Vec<Notification> {
        // Dedup before coalescing, so a cycle whose events were all already announced
        // produces nothing rather than a summary of nothing.
        let fresh: Vec<&Event> = events
            .iter()
            .filter(|event| !self.seen.contains(&event.dedup_key()))
            .collect();

        if fresh.is_empty() {
            return Vec::new();
        }

        // Recorded whether or not it is delivered: a suppressed event was on screen while
        // the user was looking at it, so announcing it later would be worse than never.
        for event in &fresh {
            self.seen.record(event.dedup_key(), now);
        }
        self.seen.save(now);

        if !self.gate.allows(focused) {
            return Vec::new();
        }
        coalesce(&fresh)
    }
}

/// Collapse one cycle's events into the messages to deliver.
fn coalesce(events: &[&Event]) -> Vec<Notification> {
    let Some(first) = events.first() else {
        return Vec::new();
    };

    if events.len() > COALESCE_ABOVE {
        return vec![Notification {
            title: first.filter_name.clone(),
            body: summarise(events),
            filter: Some(first.filter),
        }];
    }

    events
        .iter()
        .map(|event| Notification {
            title: event.filter_name.clone(),
            body: describe(event),
            filter: Some(event.filter),
        })
        .collect()
}

/// "5 new merge requests", or "5 updates" when the kinds are mixed.
fn summarise(events: &[&Event]) -> String {
    let count = events.len();
    let first = events[0].trigger.kind();
    let uniform = events.iter().all(|e| e.trigger.kind() == first);

    if !uniform {
        return format!("{count} updates");
    }
    match events[0].trigger {
        Trigger::NewMergeRequest => format!("{count} new merge requests"),
        Trigger::Approval { .. } => format!("{count} new approvals"),
        Trigger::Discussion { .. } => format!("{count} merge requests with new discussions"),
        Trigger::Pipeline { .. } => format!("{count} pipeline changes"),
        Trigger::MergedOrClosed { .. } => format!("{count} merged or closed"),
    }
}

/// One event as a line of prose.
fn describe(event: &Event) -> String {
    let mr = format!("{}!{}", event.project_name, event.iid);

    match &event.trigger {
        Trigger::NewMergeRequest => format!("{mr} {}", truncate(&event.title)),
        Trigger::Approval { by } => match by.as_slice() {
            [one] => format!("{one} approved {mr}"),
            names => format!("{} approved {mr}", names.join(", ")),
        },
        Trigger::Discussion { added } => {
            let plural = if *added == 1 {
                "discussion"
            } else {
                "discussions"
            };
            format!("{added} new {plural} on {mr}")
        }
        Trigger::Pipeline { to, .. } => match to {
            Some(status) => format!("pipeline {} on {mr}", status.label()),
            None => format!("pipeline removed on {mr}"),
        },
        Trigger::MergedOrClosed { state } => format!("{mr} was {}", state.label()),
    }
}

/// Shorten a merge-request title to something a notification can show.
fn truncate(title: &str) -> String {
    let mut out = String::with_capacity(MAX_TITLE + 1);
    for (count, c) in title.chars().enumerate() {
        if count >= MAX_TITLE {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitlab::model::{MrState, PipelineStatus};

    const FILTER: crate::app::event::FilterId = 2;

    fn now() -> Timestamp {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn gate(only_when_unfocused: bool, focus_events: bool) -> Gate {
        Gate {
            only_when_unfocused,
            focus_events,
        }
    }

    /// An event as `diff` produces it.
    fn event(id: &str, trigger: Trigger) -> Event {
        Event {
            trigger,
            filter: FILTER,
            filter_name: "Reviewing".to_owned(),
            id: id.to_owned(),
            iid: "482".to_owned(),
            project_name: "web-app".to_owned(),
            title: "Add dark mode toggle".to_owned(),
        }
    }

    fn arrivals(count: usize) -> Vec<Event> {
        (0..count)
            .map(|i| event(&format!("id-{i}"), Trigger::NewMergeRequest))
            .collect()
    }

    fn notifier(gate: Gate) -> Notifier {
        Notifier::new(gate, Seen::ephemeral())
    }

    /// Unfocused and permissive, so a test that cares about a different rule is not
    /// silently gated.
    fn delivering() -> Notifier {
        notifier(gate(false, true))
    }

    fn bodies(notifications: &[Notification]) -> Vec<&str> {
        notifications.iter().map(|n| n.body.as_str()).collect()
    }

    // ------------------------------------------------------------------- coalescing

    /// At or below the threshold, one message each.
    #[test]
    fn three_or_fewer_events_are_announced_individually() {
        let mut n = delivering();
        let got = n.process(&arrivals(3), false, now());

        assert_eq!(got.len(), 3);
        for notification in &got {
            assert_eq!(notification.title, "Reviewing");
            assert_eq!(notification.filter, Some(FILTER));
            assert!(
                notification.body.contains("web-app!482"),
                "{:?}",
                notification.body
            );
        }
    }

    /// More than three collapse into one message naming the filter and the count — a
    /// morning's backlog should be a line, not fifteen popups.
    #[test]
    fn more_than_three_events_collapse_into_a_summary() {
        let mut n = delivering();
        let got = n.process(&arrivals(5), false, now());

        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "Reviewing", "the summary names the filter");
        assert_eq!(got[0].body, "5 new merge requests");
        assert_eq!(got[0].filter, Some(FILTER));
    }

    #[test]
    fn the_threshold_is_exactly_three() {
        let mut at = delivering();
        assert_eq!(at.process(&arrivals(3), false, now()).len(), 3);

        let mut above = delivering();
        assert_eq!(above.process(&arrivals(4), false, now()).len(), 1);
    }

    /// A summary of several kinds cannot claim they are all the same thing.
    #[test]
    fn a_mixed_summary_does_not_name_one_kind() {
        let mut n = delivering();
        let events = vec![
            event("a", Trigger::NewMergeRequest),
            event("b", Trigger::NewMergeRequest),
            event(
                "c",
                Trigger::Approval {
                    by: vec!["jdoe".into()],
                },
            ),
            event("d", Trigger::Discussion { added: 1 }),
        ];

        let got = n.process(&events, false, now());
        assert_eq!(bodies(&got), ["4 updates"]);
    }

    #[test]
    fn each_uniform_kind_has_its_own_summary_wording() {
        let cases = [
            (Trigger::NewMergeRequest, "4 new merge requests"),
            (
                Trigger::Approval {
                    by: vec!["x".into()],
                },
                "4 new approvals",
            ),
            (
                Trigger::Discussion { added: 1 },
                "4 merge requests with new discussions",
            ),
            (
                Trigger::Pipeline {
                    from: None,
                    to: Some(PipelineStatus::Failed),
                },
                "4 pipeline changes",
            ),
            (
                Trigger::MergedOrClosed {
                    state: MrState::Merged,
                },
                "4 merged or closed",
            ),
        ];

        for (trigger, expected) in cases {
            let events: Vec<Event> = (0..4)
                .map(|i| event(&format!("{expected}-{i}"), trigger.clone()))
                .collect();
            let mut n = delivering();
            assert_eq!(bodies(&n.process(&events, false, now())), [expected]);
        }
    }

    #[test]
    fn no_events_produce_no_notifications() {
        assert!(delivering().process(&[], false, now()).is_empty());
    }

    // ------------------------------------------------------------------ message text

    #[test]
    fn each_trigger_reads_as_prose() {
        let cases = [
            (Trigger::NewMergeRequest, "web-app!482 Add dark mode toggle"),
            (
                Trigger::Approval {
                    by: vec!["jdoe".into()],
                },
                "jdoe approved web-app!482",
            ),
            (
                Trigger::Approval {
                    by: vec!["jdoe".into(), "bwayne".into()],
                },
                "jdoe, bwayne approved web-app!482",
            ),
            (
                Trigger::Discussion { added: 1 },
                "1 new discussion on web-app!482",
            ),
            (
                Trigger::Discussion { added: 3 },
                "3 new discussions on web-app!482",
            ),
            (
                Trigger::Pipeline {
                    from: Some(PipelineStatus::Running),
                    to: Some(PipelineStatus::Failed),
                },
                "pipeline failed on web-app!482",
            ),
            (
                Trigger::Pipeline {
                    from: Some(PipelineStatus::Running),
                    to: None,
                },
                "pipeline removed on web-app!482",
            ),
            (
                Trigger::MergedOrClosed {
                    state: MrState::Merged,
                },
                "web-app!482 was merged",
            ),
            (
                Trigger::MergedOrClosed {
                    state: MrState::Closed,
                },
                "web-app!482 was closed",
            ),
        ];

        for (trigger, expected) in cases {
            assert_eq!(describe(&event("x", trigger)), expected);
        }
    }

    /// A notification body is not a place for a 200-character title.
    #[test]
    fn a_long_title_is_truncated() {
        let mut long = event("x", Trigger::NewMergeRequest);
        long.title = "x".repeat(300);

        let body = describe(&long);
        assert!(body.chars().count() < 100, "{} chars", body.chars().count());
        assert!(body.ends_with('…'), "{body}");
    }

    /// Truncation counts characters, not bytes, or a multi-byte title would panic on a
    /// split inside a code point.
    #[test]
    fn truncation_does_not_split_a_character() {
        assert_eq!(truncate("héllo"), "héllo");
        let wide = "日".repeat(200);
        let out = truncate(&wide);
        assert_eq!(out.chars().count(), MAX_TITLE + 1, "{out}");
    }

    // ----------------------------------------------------------------- focus gating

    /// The default. A notification about something already on screen is
    /// noise.
    #[test]
    fn only_when_unfocused_suppresses_while_focused() {
        let mut n = notifier(gate(true, true));
        assert!(n.process(&arrivals(2), true, now()).is_empty());

        let mut n = notifier(gate(true, true));
        assert_eq!(n.process(&arrivals(2), false, now()).len(), 2);
    }

    #[test]
    fn the_setting_off_delivers_while_focused() {
        let mut n = notifier(gate(false, true));
        assert_eq!(n.process(&arrivals(2), true, now()).len(), 2);
    }

    /// With no focus events the setting is ignored rather than applied
    /// pessimistically. Treating an undetected terminal as permanently focused would
    /// silently suppress everything the user asked for.
    #[test]
    fn a_terminal_without_focus_events_ignores_the_setting_rather_than_suppressing() {
        let mut n = notifier(gate(true, false));

        assert_eq!(
            n.process(&arrivals(2), true, now()).len(),
            2,
            "nothing may be suppressed silently"
        );
        assert!(
            notifier(gate(true, false)).ignores_focus_setting(),
            "and the caller can say so at startup"
        );
        assert!(!notifier(gate(true, true)).ignores_focus_setting());
        assert!(!notifier(gate(false, false)).ignores_focus_setting());
    }

    /// A suppressed event was on screen while the user was looking at it, so it is
    /// recorded as seen. Announcing it when they later alt-tab away would be worse than
    /// never announcing it.
    #[test]
    fn a_suppressed_event_is_still_recorded_as_seen() {
        let mut n = notifier(gate(true, true));
        let events = arrivals(2);

        assert!(n.process(&events, true, now()).is_empty());
        assert_eq!(n.seen_count(), 2);

        assert!(
            n.process(&events, false, now()).is_empty(),
            "not re-announced once focus is lost"
        );
    }

    // ------------------------------------------------------------------------ dedup

    #[test]
    fn an_event_is_announced_once_within_a_session() {
        let mut n = delivering();
        let events = arrivals(2);

        assert_eq!(n.process(&events, false, now()).len(), 2);
        assert!(n.process(&events, false, now()).is_empty(), "not twice");
    }

    /// The key is `id` + kind, so two different changes to one merge request are two
    /// notifications and neither hides the other.
    #[test]
    fn different_kinds_on_one_merge_request_are_both_announced() {
        let mut n = delivering();

        let arrival = vec![event("same-id", Trigger::NewMergeRequest)];
        let approval = vec![event(
            "same-id",
            Trigger::Approval {
                by: vec!["jdoe".into()],
            },
        )];

        assert_eq!(n.process(&arrival, false, now()).len(), 1);
        assert_eq!(
            n.process(&approval, false, now()).len(),
            1,
            "a different kind on the same MR is a different event"
        );
        assert!(n.process(&approval, false, now()).is_empty());
    }

    /// The key excludes the filter, so an MR matching two of the user's filters is
    /// announced once rather than once per tab.
    #[test]
    fn an_event_reaching_two_filters_is_announced_once() {
        let mut n = delivering();

        let from_one = vec![event("shared", Trigger::NewMergeRequest)];
        let mut from_two = event("shared", Trigger::NewMergeRequest);
        from_two.filter = 5;
        from_two.filter_name = "Assigned".to_owned();

        assert_eq!(n.process(&from_one, false, now()).len(), 1);
        assert!(n.process(&[from_two], false, now()).is_empty());
    }

    /// A cycle whose events were all announced already produces nothing — not a summary
    /// of nothing, which is what dedup-after-coalesce would give.
    #[test]
    fn an_entirely_seen_cycle_produces_no_summary() {
        let mut n = delivering();
        let events = arrivals(5);

        assert_eq!(n.process(&events, false, now()).len(), 1);
        assert!(n.process(&events, false, now()).is_empty());
    }

    /// And a cycle where only some are new coalesces on the new ones only.
    #[test]
    fn coalescing_counts_only_the_unseen_events() {
        let mut n = delivering();
        let first = arrivals(3);
        assert_eq!(n.process(&first, false, now()).len(), 3);

        // Five events, three of them already announced.
        let mut second = first.clone();
        second.push(event("new-a", Trigger::NewMergeRequest));
        second.push(event("new-b", Trigger::NewMergeRequest));

        let got = n.process(&second, false, now());
        assert_eq!(
            got.len(),
            2,
            "two new ones, individually: {:?}",
            bodies(&got)
        );
    }

    // ------------------------------------------------------------- persistence

    fn state_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// The store persists so a restart does not re-announce.
    #[test]
    fn the_dedup_store_survives_a_restart() {
        let dir = state_dir();
        let events = arrivals(2);

        let mut first = Notifier::new(gate(false, true), Seen::load(dir.path(), now()));
        assert_eq!(first.process(&events, false, now()).len(), 2);

        let mut restarted = Notifier::new(gate(false, true), Seen::load(dir.path(), now()));
        assert_eq!(restarted.seen_count(), 2, "loaded from disk");
        assert!(
            restarted.process(&events, false, now()).is_empty(),
            "a restart must not re-announce"
        );
    }

    #[test]
    fn a_missing_store_starts_empty() {
        let dir = state_dir();
        assert!(Seen::load(dir.path(), now()).is_empty());
    }

    /// Corrupt is treated as empty: the cost is one repeated notification, and refusing
    /// to start over a dedup file would be absurd.
    #[test]
    fn a_corrupt_store_starts_empty_rather_than_failing() {
        let dir = state_dir();
        let path = dir.path().join("notified.json");

        for garbage in ["", "{", "null", "not json", r#"{"version":99,"seen":{}}"#] {
            std::fs::write(&path, garbage).unwrap();
            assert!(
                Seen::load(dir.path(), now()).is_empty(),
                "accepted {garbage:?}"
            );
        }
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = state_dir();
        let mut seen = Seen::load(dir.path(), now());
        seen.record("a:new_mr".into(), now());
        seen.save(now());

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["notified.json"], "{names:?}");
    }

    /// A session with no state directory still dedups within itself; it just forgets on
    /// exit rather than failing.
    #[test]
    fn an_ephemeral_store_dedups_without_a_file() {
        let mut n = Notifier::new(gate(false, true), Seen::ephemeral());
        let events = arrivals(1);

        assert_eq!(n.process(&events, false, now()).len(), 1);
        assert!(n.process(&events, false, now()).is_empty());
    }

    // ------------------------------------------------------------------- pruning

    /// The store only ever gains entries, so it has to shed them too.
    #[test]
    fn entries_past_the_age_limit_are_pruned() {
        let dir = state_dir();
        let old = now() - MAX_AGE - SignedDuration::from_hours(1);

        let mut seen = Seen::load(dir.path(), old);
        seen.record("stale:new_mr".into(), old);
        seen.record("fresh:new_mr".into(), now());
        seen.save(now());

        let reloaded = Seen::load(dir.path(), now());
        assert!(!reloaded.contains("stale:new_mr"), "the old key survived");
        assert!(reloaded.contains("fresh:new_mr"));
    }

    #[test]
    fn the_store_is_capped_and_drops_the_oldest_first() {
        let dir = state_dir();
        let mut seen = Seen::load(dir.path(), now());

        // Oldest first, so the ones that should survive are the high-numbered ones.
        let total = MAX_ENTRIES + 50;
        for i in 0..total {
            let age = SignedDuration::from_secs((total - i) as i64);
            seen.record(format!("key-{i:05}:new_mr"), now() - age);
        }
        seen.save(now());

        let reloaded = Seen::load(dir.path(), now());
        assert_eq!(reloaded.len(), MAX_ENTRIES);
        assert!(
            !reloaded.contains("key-00000:new_mr"),
            "the oldest should have gone"
        );
        assert!(
            reloaded.contains(&format!("key-{:05}:new_mr", total - 1)),
            "the newest should have stayed"
        );
    }

    /// Re-announcing does not refresh the timestamp, or a key seen repeatedly would never
    /// age out and the cap would evict genuinely newer ones instead.
    #[test]
    fn recording_a_known_key_keeps_its_original_timestamp() {
        let mut seen = Seen::ephemeral();
        let first = now() - SignedDuration::from_hours(10);

        seen.record("a:new_mr".into(), first);
        seen.record("a:new_mr".into(), now());

        assert_eq!(seen.seen["a:new_mr"], first);
    }
}
