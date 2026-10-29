//! When a merge request became READY, reconstructed from its history.
//!
//! READY is a state of the merge request now (see [`MergeRequest::meets`]), but the age the
//! READY column shows needs the moment each condition last started to hold. GraphQL carries
//! neither, so this is two REST calls per ready merge request:
//!
//! - `resource_label_events`, for when each required label was last added;
//! - the notes, for the system note GitLab writes when the draft is lifted —
//!   `marked this merge request as **ready**`, also written when a `Draft:` title prefix is
//!   removed.
//!
//! The pipeline is deliberately not part of it: it gates READY, but a re-run after a push must
//! not reset how long the merge request has been waiting.
//!
//! A merge request with no such event is taken to have held the condition since it was opened:
//! a label set at creation still has an event, so this only covers "never was a draft".

use std::collections::{HashMap, HashSet};

use futures_util::{StreamExt, stream};
use jiff::Timestamp;
use serde::Deserialize;

use crate::config::schema::ReadyRule;
use crate::error::Result;
use crate::gitlab::client::Client;
use crate::gitlab::model::MergeRequest;

/// Label events are oldest first with no way to reverse them, so the latest add can be on a
/// later page. Bounded like `MAX_PAGES` in `fetch`, against a server that never stops paging.
const LABEL_EVENT_PAGES: u32 = 5;

const PER_PAGE: usize = 100;

/// History lookups in flight at once for one filter, on top of the permit the refresh holds.
const CONCURRENCY: usize = 4;

/// Resolved `ready_since` per merge request id, with the `updated_at` it was resolved at.
///
/// Labelling a merge request or lifting its draft bumps `updated_at`, so an unchanged one
/// cannot have a different answer and is not looked up again. Held by the refresh worker
/// across refreshes.
pub type Memo = HashMap<String, (Timestamp, Timestamp)>;

/// The body of the system note GitLab writes when a merge request stops being a draft.
const MARKED_READY: &str = "marked this merge request as **ready**";

#[derive(Debug, Clone, Deserialize)]
pub struct LabelEvent {
    pub created_at: Timestamp,
    pub action: String,
    /// `null` once the label has been deleted from the project.
    pub label: Option<EventLabel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventLabel {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Note {
    pub created_at: Timestamp,
    pub body: String,
    pub system: bool,
}

/// Decide `ready` for every row and resolve `ready_since` for the ready ones.
///
/// A lookup that fails leaves `ready_since` unset — the READY cell reads `?` — rather than
/// failing the refresh: the rows are still worth showing.
pub async fn mark(client: &Client, rule: &ReadyRule, rows: &mut [MergeRequest], memo: &mut Memo) {
    let mut wanted = Vec::new();
    for (i, mr) in rows.iter_mut().enumerate() {
        mr.ready = mr.meets(rule);
        mr.ready_since = None;
        if !mr.ready {
            continue;
        }
        match memo.get(&mr.id) {
            Some(&(seen, at)) if seen == mr.updated_at => mr.ready_since = Some(at),
            _ => wanted.push(i),
        }
    }

    let found: Vec<(usize, Result<Timestamp>)> = {
        let rows: &[MergeRequest] = rows;
        stream::iter(wanted)
            .map(|i| async move { (i, since(client, &rows[i], rule).await) })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await
    };

    for (i, result) in found {
        let mr = &mut rows[i];
        match result {
            Ok(at) => {
                mr.ready_since = Some(at);
                memo.insert(mr.id.clone(), (mr.updated_at, at));
            }
            Err(error) => {
                tracing::warn!(project = %mr.project_path, iid = %mr.iid, %error, "could not read when the merge request became ready");
            }
        }
    }

    // Only what is ready now is worth remembering; the rest would grow without bound.
    let ready: HashSet<&str> = rows
        .iter()
        .filter(|m| m.ready)
        .map(|m| m.id.as_str())
        .collect();
    memo.retain(|id, _| ready.contains(id.as_str()));
}

/// Seed a [`Memo`] from rows resolved by an earlier run, so a restart does not look every
/// ready merge request up again.
pub fn remembered<'a>(rows: impl IntoIterator<Item = &'a MergeRequest>) -> Memo {
    rows.into_iter()
        .filter(|m| m.ready)
        .filter_map(|m| Some((m.id.clone(), (m.updated_at, m.ready_since?))))
        .collect()
}

/// Fetch the history of one merge request and resolve when it became ready.
pub async fn since(client: &Client, mr: &MergeRequest, rule: &ReadyRule) -> Result<Timestamp> {
    let base = format!(
        "projects/{}/merge_requests/{}",
        // The only character in a full path that needs escaping in a path segment.
        mr.project_path.replace('/', "%2F"),
        mr.iid
    );

    let mut events = Vec::new();
    for page in 1..=LABEL_EVENT_PAGES {
        let batch: Vec<LabelEvent> = client
            .get_rest(&format!(
                "{base}/resource_label_events?per_page={PER_PAGE}&page={page}"
            ))
            .await?;
        let last = batch.len() < PER_PAGE;
        events.extend(batch);
        if last {
            break;
        }
    }

    // Newest first, so the latest "ready" note is on the first page. One with more than a
    // page of notes since it was undrafted falls back to its creation, which overstates the
    // wait rather than hiding it.
    let notes: Vec<Note> = client
        .get_rest(&format!(
            "{base}/notes?sort=desc&order_by=created_at&per_page={PER_PAGE}"
        ))
        .await?;

    Ok(resolve(mr.created_at, &rule.labels, &events, &notes))
}

/// The later of the draft being lifted and each required label being added.
pub fn resolve(
    created_at: Timestamp,
    labels: &[String],
    events: &[LabelEvent],
    notes: &[Note],
) -> Timestamp {
    let undrafted = notes
        .iter()
        .filter(|n| n.system && n.body.trim() == MARKED_READY)
        .map(|n| n.created_at)
        .max()
        .unwrap_or(created_at);

    labels
        .iter()
        .map(|wanted| {
            events
                .iter()
                .filter(|e| e.action == "add")
                .filter(|e| e.label.as_ref().is_some_and(|l| l.name == *wanted))
                .map(|e| e.created_at)
                .max()
                .unwrap_or(created_at)
        })
        .fold(undrafted, Timestamp::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Gitlab;
    use crate::config::token::{TokenEnv, resolve as resolve_token};
    use crate::gitlab::model::fixtures::mr;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const LABEL: &str = "sre-review::ask";

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn event(at: &str, action: &str, label: &str) -> LabelEvent {
        LabelEvent {
            created_at: ts(at),
            action: action.into(),
            label: Some(EventLabel { name: label.into() }),
        }
    }

    fn note(at: &str, body: &str) -> Note {
        Note {
            created_at: ts(at),
            body: body.into(),
            system: true,
        }
    }

    const CREATED: &str = "2026-10-01T00:00:00Z";

    #[test]
    fn the_later_of_label_and_undraft_wins() {
        let labels = [LABEL.to_owned()];
        let events = [event("2026-10-02T00:00:00Z", "add", LABEL)];
        let notes = [note("2026-10-03T00:00:00Z", MARKED_READY)];
        assert_eq!(
            resolve(ts(CREATED), &labels, &events, &notes),
            ts("2026-10-03T00:00:00Z")
        );

        let events = [event("2026-10-04T00:00:00Z", "add", LABEL)];
        assert_eq!(
            resolve(ts(CREATED), &labels, &events, &notes),
            ts("2026-10-04T00:00:00Z")
        );
    }

    #[test]
    fn a_label_re_added_counts_from_the_last_add() {
        let labels = [LABEL.to_owned()];
        let events = [
            event("2026-10-02T00:00:00Z", "add", LABEL),
            event("2026-10-03T00:00:00Z", "remove", LABEL),
            event("2026-10-05T00:00:00Z", "add", LABEL),
            event("2026-10-06T00:00:00Z", "add", "unrelated"),
        ];
        assert_eq!(
            resolve(ts(CREATED), &labels, &events, &[]),
            ts("2026-10-05T00:00:00Z")
        );
    }

    #[test]
    fn redrafting_and_undrafting_counts_from_the_last_undraft() {
        let notes = [
            note("2026-10-04T00:00:00Z", MARKED_READY),
            note(
                "2026-10-03T00:00:00Z",
                "marked this merge request as **draft**",
            ),
            note("2026-10-02T00:00:00Z", MARKED_READY),
        ];
        assert_eq!(
            resolve(ts(CREATED), &[], &[], &notes),
            ts("2026-10-04T00:00:00Z")
        );
    }

    #[test]
    fn no_history_means_since_creation() {
        let labels = [LABEL.to_owned()];
        let human = Note {
            system: false,
            ..note("2026-10-09T00:00:00Z", MARKED_READY)
        };
        assert_eq!(resolve(ts(CREATED), &labels, &[], &[human]), ts(CREATED));
    }

    #[test]
    fn every_required_label_counts() {
        let labels = [LABEL.to_owned(), "team::sre".to_owned()];
        let events = [
            event("2026-10-02T00:00:00Z", "add", LABEL),
            event("2026-10-07T00:00:00Z", "add", "team::sre"),
        ];
        assert_eq!(
            resolve(ts(CREATED), &labels, &events, &[]),
            ts("2026-10-07T00:00:00Z")
        );
    }

    fn client_for(server: &MockServer) -> Client {
        let gitlab = Gitlab {
            url: server.uri(),
            token: Some("glpat-test".into()),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        let token = resolve_token(&gitlab, &TokenEnv::default(), None)
            .unwrap()
            .token;
        Client::new(&gitlab, token).unwrap()
    }

    const BASE: &str = "/api/v4/projects/acme%2Fweb%2Fweb-app/merge_requests/482";

    fn rule() -> ReadyRule {
        ReadyRule {
            labels: vec![LABEL.to_owned()],
            pipeline: true,
        }
    }

    #[tokio::test]
    async fn since_reads_both_endpoints() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{BASE}/resource_label_events")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "created_at": "2026-10-28T16:00:54.168Z",
                "action": "add",
                "label": {"name": LABEL}
            }])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{BASE}/notes")))
            .and(query_param("sort", "desc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"created_at": "2026-10-29T07:37:11.684Z", "system": true, "body": MARKED_READY},
                {"created_at": "2026-10-28T15:59:38.709Z", "system": true, "body": "changed the description"}
            ])))
            .mount(&server)
            .await;

        let found = since(&client_for(&server), &mr("gid://1", "asmith"), &rule())
            .await
            .unwrap();
        assert_eq!(found, ts("2026-10-29T07:37:11.684Z"));
    }

    #[tokio::test]
    async fn mark_looks_up_only_ready_rows_and_remembers_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{BASE}/resource_label_events")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "created_at": "2026-10-28T16:00:00Z", "action": "add", "label": {"name": LABEL}
            }])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{BASE}/notes")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&server)
            .await;

        let mut ready = mr("gid://ready", "asmith");
        ready.labels.push(crate::gitlab::model::Label {
            title: LABEL.into(),
            color: None,
        });
        ready.pipeline.as_mut().unwrap().status = crate::gitlab::model::PipelineStatus::Success;
        let mut rows = vec![ready, mr("gid://other", "asmith")];

        let client = client_for(&server);
        let mut memo = Memo::new();
        mark(&client, &rule(), &mut rows, &mut memo).await;
        assert!(rows[0].ready);
        assert_eq!(rows[0].ready_since, Some(ts("2026-10-28T16:00:00Z")));
        assert!(!rows[1].ready, "unlabelled, so not ready");

        // Unchanged, so answered from the memo: the mocks' `expect(1)` fails otherwise.
        mark(&client, &rule(), &mut rows, &mut memo).await;
        assert_eq!(rows[0].ready_since, Some(ts("2026-10-28T16:00:00Z")));
        assert_eq!(remembered(&rows), memo);
    }

    #[tokio::test]
    async fn a_server_error_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let found = since(&client_for(&server), &mr("gid://1", "asmith"), &rule()).await;
        assert!(found.is_err());
    }
}
