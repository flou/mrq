//! Row ordering.
//!
//! Sorting is client-side over a fetched snapshot, so the comparators live
//! here and the fetch layer always asks GitLab for `UPDATED_DESC`.
//!
//! # The tiebreak is not optional
//!
//! Every sort ends with `updated_at desc, id`. Without it, rows with equal keys — and
//! most keys tie often, `assigned` has two values — are ordered by whatever the previous
//! step left behind. The visible effect is rows swapping places on a background refresh
//! that changed nothing, which looks like data churn and makes the list impossible to
//! read while it updates.

use std::cmp::Ordering;

use crate::config::schema::{Column, Order};
use crate::gitlab::model::MergeRequest;

/// Order rows in place.
///
/// `drafts_last` sinks drafts below everything else regardless of the active sort, which
/// is what keeps a long tail of drafts from burying the merge requests that need action.
pub fn sort(rows: &mut [&MergeRequest], column: Column, order: Order, drafts_last: bool) {
    rows.sort_by(|a, b| {
        if drafts_last {
            // Compared before the column so it partitions the list rather than merely
            // influencing it.
            match a.draft.cmp(&b.draft) {
                Ordering::Equal => {}
                other => return other,
            }
        }

        let primary = match order {
            Order::Asc => compare(a, b, column),
            Order::Desc => compare(b, a, column),
        };
        primary.then_with(|| tiebreak(a, b))
    });
}

/// The stable final ordering: most recently updated first, then by id.
///
/// The id is the last resort and is always unique, which is what makes the whole
/// comparator a total order — and therefore the result reproducible.
fn tiebreak(a: &MergeRequest, b: &MergeRequest) -> Ordering {
    b.updated_at
        .cmp(&a.updated_at)
        .then_with(|| a.id.cmp(&b.id))
}

/// Compare two rows by one column, ascending.
fn compare(a: &MergeRequest, b: &MergeRequest, column: Column) -> Ordering {
    match column {
        // Approved first: a merge request with nothing outstanding sorts ahead of one
        // still waiting, which is also true of a merge request with no approval rules,
        // since GitLab reports those as approved.
        Column::Approved => (!a.approved).cmp(&!b.approved),

        Column::Author => case_insensitive(&a.author.username, &b.author.username),
        Column::Repo => case_insensitive(&a.project_name, &b.project_name),
        Column::Title => case_insensitive(&a.title, &b.title),

        Column::Pipeline => pipeline_severity(a).cmp(&pipeline_severity(b)),

        // Assigned-to-me first, so the column sorts the way the user means it.
        Column::Assigned => b.assigned_to_me().cmp(&a.assigned_to_me()),

        Column::Approver => people_order(&a.approved_by, &b.approved_by),
        Column::Reviewer => people_order(&a.reviewers, &b.reviewers),

        // Oldest first for AGE: an ascending age column should put the merge request
        // that has been waiting longest at the top, and that is the earliest timestamp.
        Column::Age => a.created_at.cmp(&b.created_at),
        Column::Updated => a.updated_at.cmp(&b.updated_at),

        Column::Diff => a.diff_size().cmp(&b.diff_size()),

        Column::Branch => case_insensitive(&a.source_branch, &b.source_branch),
    }
}

/// Case-insensitive, falling back to a case-sensitive comparison for stability.
///
/// Without the fallback, `Alice` and `alice` compare equal and their relative order is
/// decided by the tiebreak — correct, but surprising in a column the user is reading
/// alphabetically.
fn case_insensitive(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// Case-insensitive on the first username; nobody sorts last on ascending.
fn people_order(a: &[String], b: &[String]) -> Ordering {
    match (a.first(), b.first()) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => case_insensitive(x, y),
    }
}

/// Failed, running, pending, manual, canceled, skipped, success, none.
///
/// A merge request with no pipeline sorts last, after success — there is nothing to look
/// at, which is the least interesting state of all.
const fn pipeline_severity(mr: &MergeRequest) -> u8 {
    match &mr.pipeline {
        Some(pipeline) => pipeline.status.severity(),
        None => u8::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitlab::model::{Pipeline, PipelineStatus, User, fixtures::mr};
    use jiff::Timestamp;

    fn at(iso: &str) -> Timestamp {
        iso.parse().unwrap()
    }

    fn row(id: &str, author: &str) -> MergeRequest {
        mr(id, author)
    }

    fn ids<'a>(rows: &[&'a MergeRequest]) -> Vec<&'a str> {
        rows.iter().map(|m| m.id.as_str()).collect()
    }

    /// `sort` takes borrowed rows, matching what `ViewState::rows_of` hands it in
    /// production; tests still build owned fixtures and borrow them for sorting.
    fn refs(rows: &[MergeRequest]) -> Vec<&MergeRequest> {
        rows.iter().collect()
    }

    fn with_pipeline(id: &str, status: PipelineStatus) -> MergeRequest {
        let mut m = row(id, "someone");
        m.pipeline = Some(Pipeline {
            url: "https://example.com/p".into(),
            status,
            finished_at: None,
        });
        m
    }

    #[test]
    fn author_repo_and_title_sort_case_insensitively() {
        for column in [Column::Author, Column::Repo, Column::Title] {
            let mut mrs = vec![row("1", "zoe"), row("2", "Adam"), row("3", "mike")];
            mrs[0].project_name = "zeta".into();
            mrs[1].project_name = "Alpha".into();
            mrs[2].project_name = "mid".into();
            mrs[0].title = "zebra".into();
            mrs[1].title = "Apple".into();
            mrs[2].title = "mango".into();

            let mut rows = refs(&mrs);
            sort(&mut rows, column, Order::Asc, false);
            assert_eq!(ids(&rows), ["2", "3", "1"], "{column:?}");
        }
    }

    #[test]
    fn descending_reverses_the_order() {
        let mrs = vec![row("1", "zoe"), row("2", "adam"), row("3", "mike")];
        let mut rows = refs(&mrs);

        sort(&mut rows, Column::Author, Order::Desc, false);
        assert_eq!(ids(&rows), ["1", "3", "2"]);
    }

    /// The documented severity order; the column is useless if it does not hold.
    #[test]
    fn pipeline_sorts_by_severity() {
        let mrs = vec![
            with_pipeline("success", PipelineStatus::Success),
            with_pipeline("failed", PipelineStatus::Failed),
            with_pipeline("skipped", PipelineStatus::Skipped),
            with_pipeline("running", PipelineStatus::Running),
            with_pipeline("manual", PipelineStatus::Manual),
            with_pipeline("pending", PipelineStatus::Pending),
            with_pipeline("canceled", PipelineStatus::Canceled),
        ];
        let mut rows = refs(&mrs);

        sort(&mut rows, Column::Pipeline, Order::Asc, false);
        assert_eq!(
            ids(&rows),
            [
                "failed", "running", "pending", "manual", "canceled", "skipped", "success"
            ]
        );
    }

    /// Nothing to look at is the least interesting state, so it sorts after success.
    #[test]
    fn a_missing_pipeline_sorts_last() {
        let mut none = row("none", "someone");
        none.pipeline = None;
        let mrs = vec![none, with_pipeline("success", PipelineStatus::Success)];
        let mut rows = refs(&mrs);

        sort(&mut rows, Column::Pipeline, Order::Asc, false);
        assert_eq!(ids(&rows), ["success", "none"]);
    }

    /// Approved sorts ahead of not-approved.
    #[test]
    fn approved_sorts_ahead_of_not_approved() {
        let mut approved = row("approved", "someone");
        approved.approved = true;
        let mut pending = row("pending", "someone");
        pending.approved = false;

        let mrs = vec![pending, approved];
        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Approved, Order::Asc, false);
        assert_eq!(ids(&rows), ["approved", "pending"]);
    }

    #[test]
    fn approver_sorts_case_insensitively_and_puts_nobody_last() {
        let mut zoe = row("zoe", "someone");
        zoe.approved_by = vec!["Zoe".into()];
        let mut adam = row("adam", "someone");
        adam.approved_by = vec!["adam".into()];
        let mut nobody = row("nobody", "someone");
        nobody.approved_by = Vec::new();

        let mrs = vec![zoe, nobody, adam];
        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Approver, Order::Asc, false);
        assert_eq!(ids(&rows), ["adam", "zoe", "nobody"]);
    }

    #[test]
    fn reviewer_sorts_case_insensitively_and_puts_nobody_last() {
        let mut zoe = row("zoe", "someone");
        zoe.reviewers = vec!["Zoe".into()];
        let mut adam = row("adam", "someone");
        adam.reviewers = vec!["adam".into()];
        let mut nobody = row("nobody", "someone");
        nobody.reviewers = Vec::new();

        let mrs = vec![zoe, nobody, adam];
        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Reviewer, Order::Asc, false);
        assert_eq!(ids(&rows), ["adam", "zoe", "nobody"]);
    }

    #[test]
    fn assigned_puts_mine_first() {
        let mut mine = row("mine", "someone");
        mine.assignees = vec![User::new("me")];
        mine.recompute_derived("me");

        let theirs = row("theirs", "someone");

        let mrs = vec![theirs, mine];
        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Assigned, Order::Asc, false);
        assert_eq!(ids(&rows), ["mine", "theirs"]);
    }

    /// An ascending AGE column puts the merge request that has been waiting longest at
    /// the top, which is the earliest creation time.
    #[test]
    fn age_ascending_puts_the_oldest_first() {
        let mut old = row("old", "someone");
        old.created_at = at("2025-01-01T00:00:00Z");
        let mut new = row("new", "someone");
        new.created_at = at("2026-09-01T00:00:00Z");

        let mrs = vec![new, old];
        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Age, Order::Asc, false);
        assert_eq!(ids(&rows), ["old", "new"]);
    }

    #[test]
    fn updated_sorts_by_timestamp() {
        let mut stale = row("stale", "someone");
        stale.updated_at = at("2025-01-01T00:00:00Z");
        let mut fresh = row("fresh", "someone");
        fresh.updated_at = at("2026-09-01T00:00:00Z");

        let mrs = vec![stale, fresh];
        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Updated, Order::Desc, false);
        assert_eq!(ids(&rows), ["fresh", "stale"]);
    }

    #[test]
    fn diff_sorts_by_total_lines_touched() {
        let mut mrs = Vec::new();
        for (id, add, del) in [("big", 1000, 0), ("small", 1, 1), ("mid", 50, 50)] {
            let mut m = row(id, "someone");
            m.additions = add;
            m.deletions = del;
            mrs.push(m);
        }

        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Diff, Order::Asc, false);
        assert_eq!(ids(&rows), ["small", "mid", "big"]);
    }

    /// The property the whole tiebreak exists for: rows must not swap places on a
    /// refresh that changed nothing.
    #[test]
    fn identical_data_sorts_identically_every_time() {
        let build = || {
            let mut rows = Vec::new();
            for id in ["c", "a", "d", "b"] {
                let mut m = row(id, "someone");
                // Every key ties, so only the tiebreak decides.
                m.updated_at = at("2026-09-01T00:00:00Z");
                rows.push(m);
            }
            rows
        };

        for column in Column::DEFAULT {
            let first_mrs = build();
            let mut second_mrs = build();
            second_mrs.reverse();

            let mut first = refs(&first_mrs);
            let mut second = refs(&second_mrs);
            sort(&mut first, column, Order::Asc, false);
            sort(&mut second, column, Order::Asc, false);

            assert_eq!(
                ids(&first),
                ids(&second),
                "{column:?} depends on input order"
            );
        }
    }

    #[test]
    fn the_tiebreak_prefers_the_most_recently_updated() {
        let mut older = row("aaa", "someone");
        older.updated_at = at("2025-01-01T00:00:00Z");
        let mut newer = row("zzz", "someone");
        newer.updated_at = at("2026-09-01T00:00:00Z");

        // Assigned ties for both, so the tiebreak decides.
        let mrs = vec![older, newer];
        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Assigned, Order::Asc, false);
        assert_eq!(ids(&rows), ["zzz", "aaa"], "newest first despite the id");
    }

    /// Drafts form a second block below non-drafts, regardless of sort.
    #[test]
    fn drafts_last_partitions_the_list() {
        let mut mrs = Vec::new();
        for (id, draft) in [("d1", true), ("n1", false), ("d2", true), ("n2", false)] {
            let mut m = row(id, "someone");
            m.draft = draft;
            mrs.push(m);
        }

        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Title, Order::Asc, true);
        let order = ids(&rows);
        let first_draft = order.iter().position(|id| id.starts_with('d')).unwrap();
        let last_normal = order.iter().rposition(|id| id.starts_with('n')).unwrap();

        assert!(
            last_normal < first_draft,
            "drafts are not partitioned: {order:?}"
        );
    }

    #[test]
    fn drafts_last_applies_under_every_column_and_direction() {
        for column in Column::DEFAULT {
            for order in [Order::Asc, Order::Desc] {
                let mut mrs = Vec::new();
                for (id, draft) in [("d1", true), ("n1", false), ("d2", true)] {
                    let mut m = row(id, "someone");
                    m.draft = draft;
                    mrs.push(m);
                }

                let mut rows = refs(&mrs);
                sort(&mut rows, column, order, true);
                assert!(
                    !rows[0].draft,
                    "{column:?}/{order:?} put a draft first: {:?}",
                    ids(&rows)
                );
                assert!(rows[2].draft);
            }
        }
    }

    #[test]
    fn drafts_are_not_partitioned_when_the_setting_is_off() {
        let mut mrs = Vec::new();
        for (id, draft, title) in [("d", true, "aaa"), ("n", false, "zzz")] {
            let mut m = row(id, "someone");
            m.draft = draft;
            m.title = title.into();
            mrs.push(m);
        }

        let mut rows = refs(&mrs);
        sort(&mut rows, Column::Title, Order::Asc, false);
        assert_eq!(ids(&rows), ["d", "n"], "the draft sorts on its title");
    }

    /// Sorting must fit inside a frame.
    #[test]
    fn sorting_five_hundred_rows_is_fast() {
        let mrs: Vec<MergeRequest> = (0..500)
            .map(|i| {
                let mut m = row(&format!("id-{i:04}"), "someone");
                m.title = format!("merge request {}", (i * 7919) % 500);
                m.additions = (i % 97) as u32;
                m
            })
            .collect();

        let mut rows = refs(&mrs);
        let started = std::time::Instant::now();
        for column in Column::DEFAULT {
            sort(&mut rows, column, Order::Asc, true);
        }
        let per_sort = started.elapsed() / Column::DEFAULT.len() as u32;

        assert!(
            per_sort < std::time::Duration::from_millis(5),
            "one sort of 500 rows took {per_sort:?}, budget is 5ms"
        );
    }

    #[test]
    fn sorting_an_empty_or_single_row_list_is_a_no_op() {
        let mut empty: Vec<&MergeRequest> = Vec::new();
        sort(&mut empty, Column::Title, Order::Asc, true);
        assert!(empty.is_empty());

        let single_mr = vec![row("only", "someone")];
        let mut single = refs(&single_mr);
        sort(&mut single, Column::Title, Order::Asc, true);
        assert_eq!(ids(&single), ["only"]);
    }

    /// Every column must produce a total order, or `sort_by` can misbehave.
    #[test]
    fn every_comparator_is_a_total_order() {
        let mut rows = vec![row("a", "x"), row("b", "y"), row("c", "z")];
        rows[1].updated_at = at("2025-05-05T00:00:00Z");

        for column in Column::DEFAULT {
            for a in &rows {
                for b in &rows {
                    let forward = compare(a, b, column).then_with(|| tiebreak(a, b));
                    let backward = compare(b, a, column).then_with(|| tiebreak(b, a));
                    assert_eq!(
                        forward,
                        backward.reverse(),
                        "{column:?} is not antisymmetric for {} vs {}",
                        a.id,
                        b.id
                    );
                }
            }
        }
    }
}
