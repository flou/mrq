//! Snapshot diffing and the notification trigger rules.
//!
//! A notification is defined as a *difference* between two snapshots of one
//! filter, keyed by merge-request `id` — never as a property of the current snapshot on
//! its own. That distinction is the whole design: "this MR has two approvals" is a fact
//! about now, and telling the user about it every five minutes is what makes a tool
//! unusable. "This MR gained an approval since we last looked" can only be said by
//! comparing.
//!
//! # Silence is a type, not a flag
//!
//! The notification rules require the first fetch of a session and the cache-warm render to produce no
//! events. The dangerous way to express that is to diff against an empty previous
//! snapshot, because every row then reads as new and startup fires one notification per
//! merge request. [`Baseline::Silent`] makes "there is nothing to compare against" a
//! different thing from "the previous snapshot was empty", so the two cannot be confused
//! at a call site.
//!
//! # What this module does not do
//!
//! Coalescing, focus gating and cross-restart dedup are notification concerns too, but
//! they need terminal focus state and the state directory, so they live in `notify`
//! (mrq-4nw). This
//! module is pure: two snapshots and a config in, a list of events out, no I/O and no
//! clock. [`Event::dedup_key`] is provided here because the event kind is defined here.

use std::collections::HashMap;

use crate::app::event::FilterId;
use crate::config::schema::Notifications;
use crate::gitlab::model::{MergeRequest, MrState, PipelineStatus};

/// What changed about one merge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// An id present now and absent before.
    NewMergeRequest,

    /// An MR the user authored gained approvals. Carries the usernames that appeared, so
    /// the message can say who rather than only that the count moved.
    Approval { by: Vec<String> },

    /// An MR the user authored gained unresolved discussions.
    Discussion { added: u32 },

    /// The head pipeline's status changed. `None` on either side means the MR had no
    /// pipeline then — losing one is as much a change as gaining one.
    Pipeline {
        from: Option<PipelineStatus>,
        to: Option<PipelineStatus>,
    },

    /// The MR left the opened state.
    MergedOrClosed { state: MrState },
}

impl Trigger {
    /// The event kind, as used in the dedup key (`id` + kind).
    ///
    /// Stable strings rather than a derived `Debug`: mrq-4nw persists these across
    /// restarts, so a rename would silently re-announce everything once.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::NewMergeRequest => "new_mr",
            Self::Approval { .. } => "approval",
            Self::Discussion { .. } => "discussion",
            Self::Pipeline { .. } => "pipeline",
            Self::MergedOrClosed { .. } => "merged_or_closed",
        }
    }
}

/// One thing worth telling the user about, and enough context to say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub trigger: Trigger,

    /// Which tab it came from. Notifications fire for background tabs too, so without
    /// this the user is told something happened but not where to look.
    pub filter: FilterId,
    pub filter_name: String,

    /// The merge request, by the id the dedup key is built from.
    pub id: String,
    pub iid: String,
    pub project_name: String,
    pub title: String,
}

impl Event {
    /// The per-event dedup key persisted in the state dir.
    ///
    /// Deliberately excludes the filter: the same approval reaching the user twice because
    /// the MR matches two of their filters is the noise this key exists to prevent.
    pub fn dedup_key(&self) -> String {
        format!("{}:{}", self.id, self.trigger.kind())
    }
}

/// What to diff the current snapshot against.
#[derive(Debug, Clone, Copy)]
pub enum Baseline<'a> {
    /// Nothing to compare against, so nothing to report: the first fetch of a session, or
    /// rows restored from the cache.
    Silent,
    /// The previous snapshot of this same filter.
    Previous(&'a [MergeRequest]),
}

/// What a tab's incoming snapshot should be diffed against.
///
/// Lives here rather than at the call site because it is the one decision that turns
/// startup silent or into a burst, and the wrong answer is a plausible-looking
/// `Baseline::Previous(tab.all())` that happens to be empty.
///
/// Call it before applying the snapshot: it borrows the rows the new ones replace.
pub fn baseline_for(tab: &crate::app::state::Tab) -> Baseline<'_> {
    // The notification rules suppress "the very first fetch of a session and the cache-warm render".
    // Both are exactly "this tab has not completed a live fetch yet", which is what
    // `fetched_at` records — warm-start rows deliberately do not set it.
    if tab.fetched_at.is_none() {
        return Baseline::Silent;
    }
    Baseline::Previous(tab.all())
}

/// Evaluate the notification triggers for one filter's refresh.
///
/// Events come back in current-snapshot order, and in a fixed order per merge request, so
/// a coalesced message reads the same way twice.
pub fn diff(
    filter: FilterId,
    filter_name: &str,
    baseline: Baseline<'_>,
    current: &[MergeRequest],
    config: &Notifications,
) -> Vec<Event> {
    if !config.enabled {
        return Vec::new();
    }
    let Baseline::Previous(previous) = baseline else {
        return Vec::new();
    };

    let before: HashMap<&str, &MergeRequest> =
        previous.iter().map(|mr| (mr.id.as_str(), mr)).collect();

    let mut events = Vec::new();
    for mr in current {
        let mut push = |trigger| {
            events.push(Event {
                trigger,
                filter,
                filter_name: filter_name.to_owned(),
                id: mr.id.clone(),
                iid: mr.iid.clone(),
                project_name: mr.project_name.clone(),
                title: mr.title.clone(),
            });
        };

        let Some(before) = before.get(mr.id.as_str()) else {
            // New to this filter. Deliberately the only event it can produce: the
            // approvals and discussions it already carries are not news to anyone, and
            // announcing them alongside would make one arrival three notifications.
            if config.on_new_mr {
                push(Trigger::NewMergeRequest);
            }
            continue;
        };

        // The notification rules scope both of these to `authored_by_me`. Approvals and discussions on
        // other people's merge requests are the normal traffic of a busy instance.
        if mr.authored_by_me() {
            if config.on_approval {
                let gained: Vec<String> = mr
                    .approved_by
                    .iter()
                    .filter(|user| !before.approved_by.contains(*user))
                    .cloned()
                    .collect();
                if !gained.is_empty() {
                    push(Trigger::Approval { by: gained });
                }
            }

            if config.on_new_discussion {
                // Only an increase. A discussion being *resolved* is progress, and
                // notifying about it would punish the thing the user wants.
                let added = mr
                    .unresolved_discussions
                    .saturating_sub(before.unresolved_discussions);
                if added > 0 {
                    push(Trigger::Discussion { added });
                }
            }
        }

        if config.on_pipeline_change {
            let from = before.pipeline.as_ref().map(|p| p.status.clone());
            let to = mr.pipeline.as_ref().map(|p| p.status.clone());
            if from != to {
                push(Trigger::Pipeline { from, to });
            }
        }

        if config.on_merged_or_closed
            && mr.state != before.state
            && matches!(mr.state, MrState::Merged | MrState::Closed)
        {
            push(Trigger::MergedOrClosed { state: mr.state });
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitlab::model::{Pipeline, fixtures::mr};

    const ME: &str = "me";

    /// A merge request with the derived `*_by_me` fields computed, as `fetch` leaves them.
    fn row(id: &str, author: &str) -> MergeRequest {
        let mut row = mr(id, author);
        row.approved_by = Vec::new();
        row.unresolved_discussions = 0;
        row.pipeline = None;
        row.recompute_derived(ME);
        row
    }

    /// Authored by the current user, which is what gates the approval and discussion
    /// triggers.
    fn mine(id: &str) -> MergeRequest {
        row(id, ME)
    }

    fn theirs(id: &str) -> MergeRequest {
        row(id, "someone-else")
    }

    /// Everything on, so a test that wants a trigger off turns it off explicitly.
    fn all_on() -> Notifications {
        Notifications {
            on_pipeline_change: true,
            on_merged_or_closed: true,
            ..Notifications::default()
        }
    }

    fn pipeline(status: PipelineStatus) -> Option<Pipeline> {
        Some(Pipeline {
            url: "https://gitlab.example.com/p/1".to_owned(),
            status,
            finished_at: None,
        })
    }

    fn run(
        previous: &[MergeRequest],
        current: &[MergeRequest],
        config: &Notifications,
    ) -> Vec<Event> {
        diff(
            0,
            "Reviewing",
            Baseline::Previous(previous),
            current,
            config,
        )
    }

    fn triggers(events: &[Event]) -> Vec<&Trigger> {
        events.iter().map(|e| &e.trigger).collect()
    }

    // ---------------------------------------------------------------- baseline choice

    /// The decision that makes startup silent, tested where it is made rather than only
    /// where it is consumed: `Baseline::Previous(tab.all())` on a fresh tab compiles,
    /// reads plausibly, and fires one notification per merge request on first run.
    #[test]
    fn a_tab_with_no_completed_fetch_has_no_baseline() {
        use crate::app::state::Tabs;
        use crate::config::schema::{Filter, Scope, Sort};

        let filters = [Filter::named("Reviewing", Scope::ReviewRequested)];
        let mut tabs = Tabs::new(&filters, Sort::default(), false, None);

        let fresh = tabs.get(0).unwrap();
        assert!(
            matches!(baseline_for(fresh), Baseline::Silent),
            "a cold start must be silent"
        );

        // Warm-started from the cache: rows on screen, still no live fetch.
        tabs.get_mut(0).unwrap().apply_cached(
            vec![theirs("a")],
            std::time::Duration::from_secs(60),
            false,
            crate::gitlab::query::Fragment::full(),
        );
        assert!(
            matches!(baseline_for(tabs.get(0).unwrap()), Baseline::Silent),
            "the cache-warm render must be silent too, despite having rows"
        );

        // And the first live fetch is what starts the comparison from then on.
        tabs.get_mut(0)
            .unwrap()
            .apply_rows(vec![theirs("a")], std::time::Instant::now());
        match baseline_for(tabs.get(0).unwrap()) {
            Baseline::Previous(rows) => assert_eq!(rows.len(), 1),
            Baseline::Silent => panic!("a fetched tab has a baseline"),
        }
    }

    // ------------------------------------------------------------------- suppressions

    /// The first fetch of a session is silent. Everything would be new, and
    /// one notification per merge request at startup is the behaviour that makes a user
    /// turn notifications off for good.
    #[test]
    fn the_first_fetch_of_a_session_produces_nothing() {
        let current = vec![mine("a"), theirs("b"), theirs("c")];

        let events = diff(0, "Reviewing", Baseline::Silent, &current, &all_on());

        assert!(events.is_empty(), "{:?}", triggers(&events));
    }

    /// The cache-warm render takes the same path: rows appear on screen with no live
    /// fetch behind them, so there is nothing to have changed.
    #[test]
    fn the_cache_warm_render_produces_nothing() {
        let cached = vec![mine("a"), theirs("b")];

        let events = diff(0, "Reviewing", Baseline::Silent, &cached, &all_on());

        assert!(events.is_empty());
    }

    /// The distinction `Baseline` exists to make. Diffing against an empty previous
    /// snapshot is a real diff — the filter genuinely was empty — and must report the
    /// arrivals, which is exactly why "nothing to compare against" cannot be spelled the
    /// same way.
    #[test]
    fn an_empty_previous_snapshot_is_a_real_diff_not_a_silent_one() {
        let current = vec![theirs("a")];

        assert!(
            diff(0, "Reviewing", Baseline::Silent, &current, &all_on()).is_empty(),
            "no baseline is silent"
        );
        assert_eq!(
            triggers(&run(&[], &current, &all_on())),
            [&Trigger::NewMergeRequest],
            "an empty baseline is a filter that was empty and now is not"
        );
    }

    #[test]
    fn a_disabled_notification_config_silences_every_trigger() {
        let previous = vec![mine("a")];
        let mut current = vec![mine("a"), theirs("b")];
        current[0].approved_by = vec!["jdoe".into()];
        current[0].unresolved_discussions = 3;
        current[0].state = MrState::Merged;
        current[0].pipeline = pipeline(PipelineStatus::Failed);

        let config = Notifications {
            enabled: false,
            ..all_on()
        };
        assert!(run(&previous, &current, &config).is_empty());
    }

    #[test]
    fn an_unchanged_snapshot_produces_nothing() {
        let rows = vec![mine("a"), theirs("b")];

        assert!(run(&rows, &rows.clone(), &all_on()).is_empty());
    }

    /// A merge request leaving the filter is not an event. It cannot be told apart from
    /// one that stopped matching for any other reason — a label removed, a reviewer
    /// changed — so announcing it would be a guess.
    #[test]
    fn a_merge_request_leaving_the_filter_produces_nothing() {
        let previous = vec![mine("a"), theirs("b")];
        let current = vec![mine("a")];

        assert!(run(&previous, &current, &all_on()).is_empty());
    }

    // ----------------------------------------------------------------------- new MR

    #[test]
    fn a_new_id_is_reported_with_its_filter_and_merge_request() {
        let previous = vec![theirs("a")];
        let current = vec![theirs("a"), theirs("b")];

        let events = diff(
            2,
            "Platform",
            Baseline::Previous(&previous),
            &current,
            &all_on(),
        );

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.trigger, Trigger::NewMergeRequest);
        assert_eq!(event.id, "b");
        assert_eq!(event.filter, 2);
        assert_eq!(
            event.filter_name, "Platform",
            "the user has to know which tab to open"
        );
        assert_eq!(event.project_name, "web-app");
        assert!(!event.title.is_empty());
    }

    #[test]
    fn on_new_mr_off_suppresses_only_that_trigger() {
        let previous = vec![mine("a")];
        let mut current = vec![mine("a"), theirs("b")];
        current[0].approved_by = vec!["jdoe".into()];

        let config = Notifications {
            on_new_mr: false,
            ..all_on()
        };
        assert_eq!(
            triggers(&run(&previous, &current, &config)),
            [&Trigger::Approval {
                by: vec!["jdoe".into()]
            }],
            "the approval on the surviving MR still fires"
        );
    }

    /// One arrival is one notification. A new MR that already carries approvals and
    /// discussions must not fan out into three events about things that are not news.
    #[test]
    fn a_new_merge_request_reports_only_its_arrival() {
        let mut arrived = mine("b");
        arrived.approved_by = vec!["jdoe".into(), "bwayne".into()];
        arrived.unresolved_discussions = 4;
        arrived.pipeline = pipeline(PipelineStatus::Failed);
        arrived.state = MrState::Merged;

        let events = run(&[mine("a")], &[mine("a"), arrived], &all_on());

        assert_eq!(triggers(&events), [&Trigger::NewMergeRequest]);
    }

    // ---------------------------------------------------------------------- approvals

    #[test]
    fn an_approval_gained_on_my_merge_request_names_who_approved() {
        let previous = vec![mine("a")];
        let mut current = vec![mine("a")];
        current[0].approved_by = vec!["jdoe".into()];

        let events = run(&previous, &current, &all_on());

        assert_eq!(
            triggers(&events),
            [&Trigger::Approval {
                by: vec!["jdoe".into()]
            }]
        );
    }

    /// Two approvals in one cycle are one event naming both, not two events.
    #[test]
    fn several_approvals_in_one_cycle_are_one_event() {
        let mut previous = vec![mine("a")];
        previous[0].approved_by = vec!["jdoe".into()];
        let mut current = vec![mine("a")];
        current[0].approved_by = vec!["jdoe".into(), "bwayne".into(), "asmith".into()];

        let events = run(&previous, &current, &all_on());

        assert_eq!(
            triggers(&events),
            [&Trigger::Approval {
                by: vec!["bwayne".into(), "asmith".into()]
            }],
            "only the ones that appeared, and only once"
        );
    }

    /// Scoped to `authored_by_me`. Approvals on other people's merge requests are the
    /// normal traffic of a busy instance.
    #[test]
    fn an_approval_on_someone_elses_merge_request_is_not_reported() {
        let previous = vec![theirs("a")];
        let mut current = vec![theirs("a")];
        current[0].approved_by = vec!["jdoe".into()];

        assert!(run(&previous, &current, &all_on()).is_empty());
    }

    /// An approval that was already there is not news, however many times we refetch.
    #[test]
    fn an_approval_that_was_already_present_does_not_refire() {
        let mut rows = vec![mine("a")];
        rows[0].approved_by = vec!["jdoe".into()];

        assert!(run(&rows, &rows.clone(), &all_on()).is_empty());
    }

    /// An approval being revoked is a change, but not one the notification rules list.
    #[test]
    fn a_withdrawn_approval_is_not_reported() {
        let mut previous = vec![mine("a")];
        previous[0].approved_by = vec!["jdoe".into(), "bwayne".into()];
        let mut current = vec![mine("a")];
        current[0].approved_by = vec!["jdoe".into()];

        assert!(run(&previous, &current, &all_on()).is_empty());
    }

    #[test]
    fn on_approval_off_suppresses_it() {
        let previous = vec![mine("a")];
        let mut current = vec![mine("a")];
        current[0].approved_by = vec!["jdoe".into()];

        let config = Notifications {
            on_approval: false,
            ..all_on()
        };
        assert!(run(&previous, &current, &config).is_empty());
    }

    // -------------------------------------------------------------------- discussions

    #[test]
    fn a_new_unresolved_discussion_on_my_merge_request_carries_the_delta() {
        let mut previous = vec![mine("a")];
        previous[0].unresolved_discussions = 1;
        let mut current = vec![mine("a")];
        current[0].unresolved_discussions = 3;

        let events = run(&previous, &current, &all_on());

        assert_eq!(triggers(&events), [&Trigger::Discussion { added: 2 }]);
    }

    /// Resolving a discussion is the outcome the user wants. Notifying about it would
    /// punish progress.
    #[test]
    fn a_resolved_discussion_is_not_reported() {
        let mut previous = vec![mine("a")];
        previous[0].unresolved_discussions = 3;
        let mut current = vec![mine("a")];
        current[0].unresolved_discussions = 1;

        assert!(run(&previous, &current, &all_on()).is_empty());
    }

    #[test]
    fn a_discussion_on_someone_elses_merge_request_is_not_reported() {
        let previous = vec![theirs("a")];
        let mut current = vec![theirs("a")];
        current[0].unresolved_discussions = 5;

        assert!(run(&previous, &current, &all_on()).is_empty());
    }

    #[test]
    fn on_new_discussion_off_suppresses_it() {
        let previous = vec![mine("a")];
        let mut current = vec![mine("a")];
        current[0].unresolved_discussions = 2;

        let config = Notifications {
            on_new_discussion: false,
            ..all_on()
        };
        assert!(run(&previous, &current, &config).is_empty());
    }

    // ----------------------------------------------------------- off-by-default triggers

    /// Implemented, but off unless asked for. These fire far more
    /// often than the other three and would train the user to ignore notifications.
    #[test]
    fn pipeline_and_state_changes_are_off_in_the_default_config() {
        let defaults = Notifications::default();
        assert!(!defaults.on_pipeline_change);
        assert!(!defaults.on_merged_or_closed);

        let mut previous = vec![mine("a")];
        previous[0].pipeline = pipeline(PipelineStatus::Running);
        let mut current = vec![mine("a")];
        current[0].pipeline = pipeline(PipelineStatus::Failed);
        current[0].state = MrState::Merged;

        assert!(
            run(&previous, &current, &defaults).is_empty(),
            "neither fires without being enabled"
        );
    }

    #[test]
    fn an_enabled_pipeline_change_reports_both_ends() {
        let mut previous = vec![theirs("a")];
        previous[0].pipeline = pipeline(PipelineStatus::Running);
        let mut current = vec![theirs("a")];
        current[0].pipeline = pipeline(PipelineStatus::Failed);

        let events = run(&previous, &current, &all_on());

        assert_eq!(
            triggers(&events),
            [&Trigger::Pipeline {
                from: Some(PipelineStatus::Running),
                to: Some(PipelineStatus::Failed),
            }]
        );
    }

    /// Gaining or losing a pipeline is as much a change as one status becoming another,
    /// and a `None` on either side must not be read as "unchanged".
    #[test]
    fn gaining_or_losing_a_pipeline_is_a_change() {
        let previous = vec![theirs("a")];
        let mut gained = vec![theirs("a")];
        gained[0].pipeline = pipeline(PipelineStatus::Running);

        assert_eq!(
            triggers(&run(&previous, &gained, &all_on())),
            [&Trigger::Pipeline {
                from: None,
                to: Some(PipelineStatus::Running),
            }]
        );

        assert_eq!(
            triggers(&run(&gained, &previous, &all_on())),
            [&Trigger::Pipeline {
                from: Some(PipelineStatus::Running),
                to: None,
            }]
        );
    }

    #[test]
    fn an_unchanged_pipeline_status_is_not_reported() {
        let mut rows = vec![theirs("a")];
        rows[0].pipeline = pipeline(PipelineStatus::Running);

        assert!(run(&rows, &rows.clone(), &all_on()).is_empty());
    }

    #[test]
    fn an_enabled_merge_reports_the_new_state() {
        let previous = vec![theirs("a")];
        let mut current = vec![theirs("a")];
        current[0].state = MrState::Merged;

        assert_eq!(
            triggers(&run(&previous, &current, &all_on())),
            [&Trigger::MergedOrClosed {
                state: MrState::Merged
            }]
        );

        let mut closed = vec![theirs("a")];
        closed[0].state = MrState::Closed;
        assert_eq!(
            triggers(&run(&previous, &closed, &all_on())),
            [&Trigger::MergedOrClosed {
                state: MrState::Closed
            }]
        );
    }

    /// Only the transition fires. An MR that was already merged when we first saw it, and
    /// still is, must not be re-announced every cycle.
    #[test]
    fn an_already_merged_merge_request_does_not_refire() {
        let mut rows = vec![theirs("a")];
        rows[0].state = MrState::Merged;

        assert!(run(&rows, &rows.clone(), &all_on()).is_empty());
    }

    /// Locked is a state change but not one the notification rules name, and treating it as "closed" would
    /// tell the user something untrue.
    #[test]
    fn a_lock_is_not_a_merge_or_a_close() {
        let previous = vec![theirs("a")];
        let mut current = vec![theirs("a")];
        current[0].state = MrState::Locked;

        assert!(run(&previous, &current, &all_on()).is_empty());
    }

    // ---------------------------------------------------------------- shape and ordering

    /// Several changes to one merge request in one cycle each get their own event, in a
    /// fixed order, so a coalesced message reads the same way twice.
    #[test]
    fn one_merge_request_can_produce_several_events_in_a_fixed_order() {
        let mut previous = vec![mine("a")];
        previous[0].unresolved_discussions = 1;
        previous[0].pipeline = pipeline(PipelineStatus::Running);

        let mut current = vec![mine("a")];
        current[0].approved_by = vec!["jdoe".into()];
        current[0].unresolved_discussions = 2;
        current[0].pipeline = pipeline(PipelineStatus::Success);
        current[0].state = MrState::Merged;

        let kinds: Vec<&str> = run(&previous, &current, &all_on())
            .iter()
            .map(|e| e.trigger.kind())
            .collect();

        assert_eq!(
            kinds,
            ["approval", "discussion", "pipeline", "merged_or_closed"]
        );
    }

    #[test]
    fn events_come_back_in_current_snapshot_order() {
        let previous = vec![theirs("a")];
        let current = vec![theirs("a"), theirs("c"), theirs("b")];

        let events = run(&previous, &current, &all_on());
        let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();

        assert_eq!(ids, ["c", "b"], "snapshot order, not sorted");
    }

    /// The key is `id` + kind. It is persisted across restarts, so the kind
    /// strings are a stored format and two different changes to one MR must not collide.
    #[test]
    fn dedup_keys_are_distinct_per_kind_and_stable_per_merge_request() {
        let mut previous = vec![mine("a")];
        previous[0].pipeline = pipeline(PipelineStatus::Running);
        let mut current = vec![mine("a")];
        current[0].approved_by = vec!["jdoe".into()];
        current[0].unresolved_discussions = 1;
        current[0].pipeline = pipeline(PipelineStatus::Success);

        let events = run(&previous, &current, &all_on());
        let keys: Vec<String> = events.iter().map(Event::dedup_key).collect();

        assert_eq!(keys, ["a:approval", "a:discussion", "a:pipeline"]);

        let distinct: std::collections::BTreeSet<&String> = keys.iter().collect();
        assert_eq!(distinct.len(), keys.len(), "keys collided: {keys:?}");
    }

    /// The same change reaching the user twice because the MR matches two of their
    /// filters is the noise the key exists to prevent, so it must not vary by filter.
    #[test]
    fn a_dedup_key_does_not_vary_by_filter() {
        let previous = vec![theirs("a")];
        let current = vec![theirs("a"), theirs("b")];

        let from_one = diff(
            0,
            "Assigned",
            Baseline::Previous(&previous),
            &current,
            &all_on(),
        );
        let from_two = diff(
            1,
            "Reviewing",
            Baseline::Previous(&previous),
            &current,
            &all_on(),
        );

        assert_eq!(from_one[0].dedup_key(), from_two[0].dedup_key());
        assert_ne!(
            from_one[0].filter_name, from_two[0].filter_name,
            "but each still says which tab it came from"
        );
    }

    /// Every kind has a distinct, non-empty key string, since a duplicate would silently
    /// merge two unrelated events in the persisted dedup set.
    #[test]
    fn every_trigger_kind_is_distinct() {
        let kinds = [
            Trigger::NewMergeRequest.kind(),
            Trigger::Approval { by: Vec::new() }.kind(),
            Trigger::Discussion { added: 1 }.kind(),
            Trigger::Pipeline {
                from: None,
                to: None,
            }
            .kind(),
            Trigger::MergedOrClosed {
                state: MrState::Merged,
            }
            .kind(),
        ];

        let distinct: std::collections::BTreeSet<&str> = kinds.iter().copied().collect();
        assert_eq!(distinct.len(), kinds.len(), "{kinds:?}");
        assert!(kinds.iter().all(|k| !k.is_empty()));
    }

    /// The diff is keyed on `id`, never on `iid`, which is only unique per project
    ///. Two merge requests numbered !482 in different projects are two
    /// merge requests.
    #[test]
    fn diffing_keys_on_the_global_id_not_the_project_iid() {
        let previous = vec![theirs("gid://gitlab/MergeRequest/1")];
        let mut other_project = theirs("gid://gitlab/MergeRequest/2");
        other_project.project_name = "api".to_owned();

        let current = vec![previous[0].clone(), other_project];

        let events = run(&previous, &current, &all_on());

        assert_eq!(events.len(), 1, "the shared iid must not hide an arrival");
        assert_eq!(events[0].id, "gid://gitlab/MergeRequest/2");
        assert_eq!(events[0].iid, previous[0].iid, "same iid, different MR");
    }
}
