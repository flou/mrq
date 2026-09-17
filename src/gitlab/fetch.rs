//! Fetching one filter: pagination, and the degradation ladder around it.
//!
//! Pages are requested one at a time — never aliased several into one document,
//! because each alias multiplies the query's cost — and the loop stops at
//! `max_results` or when the connection is exhausted.
//!
//! # The order of the fallbacks
//!
//! GraphQL validates a document before analysing its cost, so a query that is both
//! over-budget and references a field the instance lacks reports only the unknown field.
//! Unknown fields are therefore dropped first, and only then is the complexity ladder
//! walked. Getting this backwards leaves the complexity ladder looking correct while
//! never firing on an instance that has both problems.
//!
//! # The ladder is sticky
//!
//! [`Degradation`] is the caller's, not this module's, and it is carried across refreshes.
//! An instance configured below the budget, or one missing a field this build assumes
//! exists, is a property of the instance rather than of one request: rediscovering it
//! every five minutes would spend a wasted round trip per filter per refresh, forever, and
//! log the same downgrade every time. Starting from the last rung that worked means the
//! ladder is walked once per session and the downgrade is logged once per filter.

use jiff::Timestamp;

use crate::config::schema::Filter;
use crate::error::{Error, Result};
use crate::gitlab::client::Client;
use crate::gitlab::model::MergeRequest;
use crate::gitlab::query::{self, FALLBACK_PAGE_SIZES, Fragment};
use crate::gitlab::wire::{Anomalies, FilterData};

/// One filter's fetched result.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub merge_requests: Vec<MergeRequest>,
    /// `true` when the server had more and `max_results` stopped us, so the UI can say
    /// the list is capped rather than implying it is complete.
    pub truncated: bool,
    /// The query shape that finally worked. A degraded one means fields are missing.
    pub fragment: Fragment,
    /// Set when a response carried errors alongside data.
    pub partial: bool,
    /// Values this build did not recognise. Already logged by the time a `Snapshot`
    /// exists; kept on it only so the integration test can assert a real instance's
    /// response is fully representable.
    #[cfg(test)]
    pub(crate) anomalies: Anomalies,
}

/// How many requests one refresh of a filter may make.
///
/// A guard against a server that keeps returning `hasNextPage: true` with a cursor that
/// does not advance — a bug we cannot fix from here, and one that would otherwise spin
/// until the process is killed.
const MAX_PAGES: usize = 50;

/// How far down the degradation ladder a filter has had to go.
///
/// Held by the caller across refreshes rather than reset per fetch — see the module note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degradation {
    page_size: u32,
    fragment: Fragment,
}

impl Degradation {
    /// The shape every filter starts on: the full fragment at the documented page size.
    pub fn none() -> Self {
        Self {
            page_size: FALLBACK_PAGE_SIZES[0],
            fragment: Fragment::full(),
        }
    }

    #[cfg(test)]
    pub(crate) const fn fragment(&self) -> &Fragment {
        &self.fragment
    }

    #[cfg(test)]
    pub(crate) fn is_degraded(&self) -> bool {
        self.fragment.is_degraded() || self.page_size != FALLBACK_PAGE_SIZES[0]
    }
}

impl Default for Degradation {
    fn default() -> Self {
        Self::none()
    }
}

/// Fetch one filter, following pagination and degrading the query if the server refuses
/// it.
///
/// `degradation` is both where the attempt starts and where it ends up: a filter that
/// needed a smaller page or a reduced fragment last time asks for that shape first.
pub async fn fetch(
    client: &Client,
    filter: &Filter,
    degradation: &mut Degradation,
    current_user: &str,
    instance_url: &str,
    now: Timestamp,
) -> Result<Snapshot> {
    loop {
        match paginate(
            client,
            filter,
            &degradation.fragment,
            degradation.page_size,
            current_user,
            instance_url,
            now,
        )
        .await
        {
            Ok(snapshot) => return Ok(snapshot),

            // Validation: the instance does not have these fields. Drop exactly the ones
            // it named and try again — dropping a whole category instead would lose
            // labels and reviewers over two missing approval scalars.
            Err(Error::UnknownField { fields }) => {
                if fields.iter().all(|f| degradation.fragment.excludes(f)) {
                    // Already excluded and still rejected: retrying would loop.
                    return Err(Error::UnknownField { fields });
                }
                // Logged here rather than per request: reaching this arm means the shape
                // changed, and a sticky `degradation` means it changes once per session.
                tracing::info!(
                    filter = %filter.name,
                    fields = %fields.join(", "),
                    "instance does not support these fields; retrying without them"
                );
                degradation.fragment = degradation.fragment.excluding(fields);
            }

            // Cost: halve the page size, then give up the nested connections.
            Err(Error::LimitExceeded { kind, limit }) => {
                match next_rung(degradation.page_size, &degradation.fragment) {
                    Some((smaller, reduced)) => {
                        tracing::warn!(
                            filter = %filter.name,
                            limit = ?limit,
                            kind = kind.as_str(),
                            page_size = smaller,
                            minimal = reduced.is_minimal(),
                            "query rejected for cost; degrading"
                        );
                        degradation.page_size = smaller;
                        degradation.fragment = reduced;
                    }
                    None => return Err(Error::LimitExceeded { kind, limit }),
                }
            }

            Err(other) => return Err(other),
        }
    }
}

/// The next step down the complexity ladder, or `None` at the bottom.
///
/// Page size is given up before fields are: a smaller page costs another round trip,
/// while a reduced fragment costs the user data they can see.
fn next_rung(page_size: u32, fragment: &Fragment) -> Option<(u32, Fragment)> {
    if let Some(smaller) = FALLBACK_PAGE_SIZES.iter().copied().find(|s| *s < page_size) {
        return Some((smaller, fragment.clone()));
    }
    if !fragment.is_minimal() {
        // Back to the top of the page-size ladder: the minimal fragment is cheap enough
        // that a small page would be needlessly slow.
        return Some((FALLBACK_PAGE_SIZES[0], Fragment::minimal()));
    }
    None
}

/// Walk the pages for one query shape.
async fn paginate(
    client: &Client,
    filter: &Filter,
    fragment: &Fragment,
    page_size: u32,
    current_user: &str,
    instance_url: &str,
    now: Timestamp,
) -> Result<Snapshot> {
    let mut merge_requests: Vec<MergeRequest> = Vec::new();
    let mut anomalies = Anomalies::default();
    let mut cursor: Option<String> = None;
    let mut partial = false;
    let mut truncated = false;

    // Whether `$after` is declared at all depends on whether a cursor is present, not on
    // its value — so the query text is fixed once a cursor exists, and only
    // `variables["after"]` changes from page to page. Rebuilding per page re-ran the same
    // handful of `format!` calls for no reason beyond the first two pages.
    let mut document = query::build(filter, fragment, page_size, cursor.as_deref(), now);

    for _ in 0..MAX_PAGES {
        let response = client
            .execute::<FilterData, _>(&document.query, &document.variables)
            .await?;

        // A response carrying both data and errors is rendered rather than discarded,
        // but the tab is marked so the user knows it is incomplete.
        if let Some(failure) = response.failure() {
            match failure {
                Error::GraphQlPartial { .. } => partial = true,
                other => return Err(other),
            }
        }

        let Some(connection) = response.data.and_then(FilterData::connection) else {
            // A null root is what a group query returns when the path does not exist or
            // is not visible to this token. An empty result would hide that.
            return Err(Error::Other(format!(
                "no merge requests returned for filter `{}`; check its scope and path",
                filter.name
            )));
        };

        let page = connection.into_page(instance_url, current_user);
        merge_requests.extend(page.merge_requests);
        merge(&mut anomalies, page.anomalies);

        if merge_requests.len() >= filter.max_results {
            merge_requests.truncate(filter.max_results);
            // Only truncated if the server actually had more; hitting the cap exactly on
            // the last page is a complete list, not a capped one.
            truncated = page.has_next_page;
            break;
        }

        if !page.has_next_page {
            break;
        }

        match page.end_cursor {
            // No cursor with more pages promised: the server contradicted itself, and
            // repeating the request would fetch the same page forever.
            None => {
                tracing::warn!(
                    filter = %filter.name,
                    "server reported more pages but returned no cursor; stopping"
                );
                break;
            }
            Some(next) if Some(&next) == cursor.as_ref() => {
                tracing::warn!(filter = %filter.name, "cursor did not advance; stopping");
                break;
            }
            Some(next) => {
                if cursor.is_none() {
                    // The one shape change: the first page declared no `$after` at all.
                    cursor = Some(next);
                    document = query::build(filter, fragment, page_size, cursor.as_deref(), now);
                } else {
                    document.variables["after"] = serde_json::Value::String(next.clone());
                    cursor = Some(next);
                }
            }
        }
    }

    if !anomalies.is_empty() {
        // Once per refresh, not once per row: a Free-tier instance would otherwise emit
        // a line per merge request every five minutes.
        tracing::info!(
            filter = %filter.name,
            pipeline_statuses = ?anomalies.unknown_pipeline_statuses,
            merge_statuses = ?anomalies.unknown_merge_statuses,
            timestamps = ?anomalies.unparsable_timestamps,
            "response contained values this build does not recognise"
        );
    }

    Ok(Snapshot {
        merge_requests,
        truncated,
        fragment: fragment.clone(),
        partial,
        #[cfg(test)]
        anomalies,
    })
}

fn merge(into: &mut Anomalies, from: Anomalies) {
    for value in from.unknown_pipeline_statuses {
        if !into.unknown_pipeline_statuses.contains(&value) {
            into.unknown_pipeline_statuses.push(value);
        }
    }
    for value in from.unknown_merge_statuses {
        if !into.unknown_merge_statuses.contains(&value) {
            into.unknown_merge_statuses.push(value);
        }
    }
    for value in from.unparsable_timestamps {
        if !into.unparsable_timestamps.contains(&value) {
            into.unparsable_timestamps.push(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Gitlab, Scope};
    use crate::config::token::{TokenEnv, resolve};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const INSTANCE: &str = "https://gitlab.example.com";

    fn now() -> Timestamp {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn client_for(server: &MockServer) -> Client {
        let gitlab = Gitlab {
            url: server.uri(),
            token: Some("glpat-test".into()),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        Client::new(&gitlab, token).unwrap()
    }

    /// A fetch from an undegraded start, for the tests that do not exercise the ladder.
    async fn fresh(client: &Client, filter: &Filter) -> Result<Snapshot> {
        fetch(
            client,
            filter,
            &mut Degradation::none(),
            "me",
            INSTANCE,
            now(),
        )
        .await
    }

    fn filter(max_results: usize) -> Filter {
        Filter {
            max_results,
            ..Filter::named("Assigned", Scope::Assigned)
        }
    }

    /// A minimal node, enough to deserialize.
    fn node(id: &str) -> Value {
        json!({
            "id": format!("gid://gitlab/MergeRequest/{id}"),
            "iid": id,
            "title": format!("MR {id}"),
            "webUrl": format!("{INSTANCE}/g/p/-/merge_requests/{id}"),
            "draft": false,
            "state": "opened",
            "createdAt": "2026-09-01T00:00:00Z",
            "updatedAt": "2026-09-10T00:00:00Z",
            "sourceBranch": "feat",
            "targetBranch": "main",
            "conflicts": false,
            "mergeStatusEnum": "CAN_BE_MERGED",
            "author": {"username": "someone"},
            "project": {"fullPath": "group/project"},
            "diffStatsSummary": {"additions": 1, "deletions": 1, "fileCount": 1},
            "resolvableDiscussionsCount": 0,
            "resolvedDiscussionsCount": 0,
            "userNotesCount": 0,
            "headPipeline": null,
            "approved": false,
            "approvedBy": {"nodes": []},
            "assignees": {"nodes": []},
            "reviewers": {"nodes": []},
            "labels": {"nodes": []}
        })
    }

    fn page(ids: &[&str], has_next: bool, cursor: Option<&str>) -> Value {
        json!({"data": {"currentUser": {"assignedMergeRequests": {
            "pageInfo": {"hasNextPage": has_next, "endCursor": cursor},
            "nodes": ids.iter().map(|id| node(id)).collect::<Vec<_>>()
        }}}})
    }

    /// Replies from a scripted list, recording each request body.
    struct Script {
        responses: Vec<Value>,
        calls: Arc<AtomicUsize>,
        bodies: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl Respond for Script {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(body) = serde_json::from_slice::<Value>(&request.body) {
                self.bodies.lock().unwrap().push(body);
            }
            let response = self
                .responses
                .get(index)
                .cloned()
                .unwrap_or_else(|| page(&[], false, None));
            ResponseTemplate::new(200).set_body_json(response)
        }
    }

    async fn serving(
        responses: Vec<Value>,
    ) -> (
        MockServer,
        Arc<AtomicUsize>,
        Arc<std::sync::Mutex<Vec<Value>>>,
    ) {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));

        Mock::given(method("POST"))
            .respond_with(Script {
                responses,
                calls: Arc::clone(&calls),
                bodies: Arc::clone(&bodies),
            })
            .mount(&server)
            .await;

        (server, calls, bodies)
    }

    #[tokio::test]
    async fn a_single_page_is_one_request() {
        let (server, calls, _) = serving(vec![page(&["1", "2"], false, None)]).await;

        let snapshot = fresh(&client_for(&server), &filter(100)).await.unwrap();

        assert_eq!(snapshot.merge_requests.len(), 2);
        assert!(!snapshot.truncated);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// One page per request, never aliased into one document.
    #[tokio::test]
    async fn pages_are_followed_one_request_at_a_time() {
        let (server, calls, bodies) = serving(vec![
            page(&["1", "2"], true, Some("CURSOR1")),
            page(&["3", "4"], true, Some("CURSOR2")),
            page(&["5"], false, None),
        ])
        .await;

        let snapshot = fresh(&client_for(&server), &filter(100)).await.unwrap();

        assert_eq!(snapshot.merge_requests.len(), 5);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let bodies = bodies.lock().unwrap();
        assert!(
            bodies[0]["variables"].get("after").is_none(),
            "the first page has no cursor"
        );
        assert_eq!(bodies[1]["variables"]["after"], "CURSOR1");
        assert_eq!(bodies[2]["variables"]["after"], "CURSOR2");
    }

    #[tokio::test]
    async fn the_first_page_uses_the_documented_page_size() {
        let (server, _, bodies) = serving(vec![page(&["1"], false, None)]).await;

        fresh(&client_for(&server), &filter(100)).await.unwrap();

        assert_eq!(
            bodies.lock().unwrap()[0]["variables"]["first"],
            FALLBACK_PAGE_SIZES[0]
        );
    }

    /// Server-side sort, so a truncated set is the most recently touched.
    #[tokio::test]
    async fn results_are_requested_in_updated_order() {
        let (server, _, bodies) = serving(vec![page(&["1"], false, None)]).await;

        fresh(&client_for(&server), &filter(100)).await.unwrap();

        let query = bodies.lock().unwrap()[0]["query"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(query.contains("sort: UPDATED_DESC"), "{query}");
    }

    #[tokio::test]
    async fn pagination_stops_at_max_results() {
        let (server, calls, _) = serving(vec![
            page(&["1", "2", "3"], true, Some("C1")),
            page(&["4", "5", "6"], true, Some("C2")),
        ])
        .await;

        let snapshot = fresh(&client_for(&server), &filter(4)).await.unwrap();

        assert_eq!(snapshot.merge_requests.len(), 4, "trimmed to the cap");
        assert!(snapshot.truncated, "the server had more");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "stopped early");
    }

    /// Hitting the cap exactly on the last page is a complete list, not a capped one —
    /// and telling the user it is capped would be wrong.
    #[tokio::test]
    async fn an_exact_fit_is_not_reported_as_truncated() {
        let (server, _, _) = serving(vec![page(&["1", "2", "3"], false, None)]).await;

        let snapshot = fresh(&client_for(&server), &filter(3)).await.unwrap();

        assert_eq!(snapshot.merge_requests.len(), 3);
        assert!(!snapshot.truncated);
    }

    #[tokio::test]
    async fn an_empty_result_is_an_empty_snapshot() {
        let (server, _, _) = serving(vec![page(&[], false, None)]).await;

        let snapshot = fresh(&client_for(&server), &filter(100)).await.unwrap();

        assert!(snapshot.merge_requests.is_empty());
        assert!(!snapshot.truncated);
        assert!(!snapshot.partial);
    }

    /// Two fields named as undefined in one response. The retry drops those and keeps
    /// everything else.
    #[tokio::test]
    async fn unknown_fields_are_dropped_and_the_fetch_retried() {
        let rejection = json!({"errors": [
            {"message": "Field 'userNotesCount' doesn't exist on type 'MergeRequest'",
             "extensions": {"code": "undefinedField", "fieldName": "userNotesCount"}},
            {"message": "Field 'resolvableDiscussionsCount' doesn't exist on type 'MergeRequest'",
             "extensions": {"code": "undefinedField", "fieldName": "resolvableDiscussionsCount"}}
        ]});
        let (server, calls, bodies) = serving(vec![rejection, page(&["1"], false, None)]).await;

        let snapshot = fresh(&client_for(&server), &filter(100)).await.unwrap();

        assert_eq!(snapshot.merge_requests.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one retry");
        assert!(snapshot.fragment.excludes("userNotesCount"));
        assert!(snapshot.fragment.is_degraded());

        let bodies = bodies.lock().unwrap();
        let retried = bodies[1]["query"].as_str().unwrap();
        assert!(!retried.contains("userNotesCount"), "{retried}");
        assert!(
            retried.contains("labels(first:"),
            "labels must survive an unrelated missing field: {retried}"
        );
    }

    /// An instance that rejects a field and then rejects it again would otherwise loop.
    #[tokio::test]
    async fn a_repeated_unknown_field_rejection_gives_up() {
        let rejection = json!({"errors": [
            {"message": "Field 'labels' doesn't exist on type 'MergeRequest'",
             "extensions": {"code": "undefinedField", "fieldName": "labels"}}
        ]});
        let (server, calls, _) = serving(vec![rejection.clone(), rejection]).await;

        let err = fresh(&client_for(&server), &filter(100)).await.unwrap_err();

        assert!(matches!(err, Error::UnknownField { .. }), "{err:?}");
        assert!(calls.load(Ordering::SeqCst) <= 3, "did not loop");
    }

    /// Halve the page size, then give up the nested connections.
    #[tokio::test]
    async fn a_complexity_rejection_walks_the_ladder() {
        let too_costly = json!({"errors": [
            {"message": "Query has complexity of 318, which exceeds max complexity of 250"}
        ]});
        let (server, calls, bodies) = serving(vec![
            too_costly.clone(),
            too_costly,
            page(&["1"], false, None),
        ])
        .await;

        let snapshot = fresh(&client_for(&server), &filter(100)).await.unwrap();

        assert_eq!(snapshot.merge_requests.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let bodies = bodies.lock().unwrap();
        let sizes: Vec<u64> = bodies
            .iter()
            .map(|b| b["variables"]["first"].as_u64().unwrap())
            .collect();
        assert!(sizes[1] < sizes[0], "page size should shrink: {sizes:?}");
    }

    /// The ladder is walked once, not once per refresh. An instance
    /// configured below the budget would otherwise spend a rejected request per filter
    /// per refresh forever, and log the same downgrade every time.
    #[tokio::test]
    async fn the_rung_that_worked_is_reused_by_the_next_refresh() {
        let too_costly = json!({"errors": [
            {"message": "Query has complexity of 318, which exceeds max complexity of 250"}
        ]});
        let (server, calls, bodies) = serving(vec![
            too_costly,
            page(&["1"], false, None),
            page(&["2"], false, None),
        ])
        .await;

        let client = client_for(&server);
        let mut degradation = Degradation::none();

        fetch(
            &client,
            &filter(100),
            &mut degradation,
            "me",
            INSTANCE,
            now(),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one rejection, one retry");
        assert!(degradation.is_degraded());

        let settled = degradation.clone();
        fetch(
            &client,
            &filter(100),
            &mut degradation,
            "me",
            INSTANCE,
            now(),
        )
        .await
        .unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "the second refresh must not re-walk the ladder"
        );
        assert_eq!(degradation, settled, "and must not move off the rung");

        let bodies = bodies.lock().unwrap();
        assert_eq!(
            bodies[1]["variables"]["first"], bodies[2]["variables"]["first"],
            "the second refresh asks for the shape that worked"
        );
    }

    /// The same argument as the complexity ladder: a missing field is a property of the
    /// instance, not of one request.
    #[tokio::test]
    async fn an_unsupported_field_is_not_re_requested_every_refresh() {
        let rejection = json!({"errors": [
            {"message": "Field 'userNotesCount' doesn't exist on type 'MergeRequest'",
             "extensions": {"code": "undefinedField", "fieldName": "userNotesCount"}}
        ]});
        let (server, calls, bodies) = serving(vec![
            rejection,
            page(&["1"], false, None),
            page(&["2"], false, None),
        ])
        .await;

        let client = client_for(&server);
        let mut degradation = Degradation::none();

        fetch(
            &client,
            &filter(100),
            &mut degradation,
            "me",
            INSTANCE,
            now(),
        )
        .await
        .unwrap();
        fetch(
            &client,
            &filter(100),
            &mut degradation,
            "me",
            INSTANCE,
            now(),
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 3, "one retry in total");
        assert!(degradation.fragment().excludes("userNotesCount"));

        let bodies = bodies.lock().unwrap();
        let second = bodies[2]["query"].as_str().unwrap();
        assert!(!second.contains("userNotesCount"), "{second}");
    }

    /// A degraded fetch still reports the shape it settled on, so the tab can be marked
    /// and the status bar can say what is missing.
    #[tokio::test]
    async fn a_degraded_snapshot_reports_what_was_lost() {
        let too_costly = json!({"errors": [
            {"message": "Query has complexity of 318, which exceeds max complexity of 250"}
        ]});
        let (server, _, _) = serving(vec![
            too_costly.clone(),
            too_costly.clone(),
            too_costly,
            page(&["1"], false, None),
        ])
        .await;

        let mut degradation = Degradation::none();
        let snapshot = fetch(
            &client_for(&server),
            &filter(100),
            &mut degradation,
            "me",
            INSTANCE,
            now(),
        )
        .await
        .unwrap();

        assert!(snapshot.fragment.is_minimal());
        let lost = snapshot
            .fragment
            .lost()
            .expect("a minimal fragment lost data");
        assert!(
            lost.contains("reviewers") && lost.contains("labels"),
            "{lost}"
        );
        assert!(
            !lost.contains("assignees"),
            "assignees survives the reduction: {lost}"
        );
    }

    /// Page size is given up before fields are: a smaller page costs a round trip, a
    /// reduced fragment costs the user data they can see.
    #[test]
    fn the_ladder_exhausts_page_sizes_before_reducing_fields() {
        let full = Fragment::full();

        let (size, fragment) = next_rung(FALLBACK_PAGE_SIZES[0], &full).unwrap();
        assert_eq!(size, FALLBACK_PAGE_SIZES[1]);
        assert!(
            !fragment.is_minimal(),
            "fields intact while page size remains"
        );

        let smallest = *FALLBACK_PAGE_SIZES.last().unwrap();
        let (size, fragment) = next_rung(smallest, &full).unwrap();
        assert!(fragment.is_minimal(), "only then are fields dropped");
        assert_eq!(size, FALLBACK_PAGE_SIZES[0], "page size resets");

        assert_eq!(
            next_rung(smallest, &Fragment::minimal()),
            None,
            "the bottom of the ladder"
        );
    }

    #[tokio::test]
    async fn an_unfixable_complexity_rejection_is_reported() {
        let too_costly = json!({"errors": [
            {"message": "Query has complexity of 9999, which exceeds max complexity of 10"}
        ]});
        let (server, _, _) = serving(vec![
            too_costly.clone(),
            too_costly.clone(),
            too_costly.clone(),
            too_costly.clone(),
            too_costly.clone(),
            too_costly,
        ])
        .await;

        let err = fresh(&client_for(&server), &filter(100)).await.unwrap_err();
        assert!(matches!(err, Error::LimitExceeded { .. }), "{err:?}");
    }

    /// Partial data is rendered, and the snapshot records that it is
    /// incomplete.
    #[tokio::test]
    async fn a_partial_response_is_kept_and_marked() {
        let partial = json!({
            "data": {"currentUser": {"assignedMergeRequests": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": [node("1")]
            }}},
            "errors": [{"message": "a resolver failed"}]
        });
        let (server, _, _) = serving(vec![partial]).await;

        let snapshot = fresh(&client_for(&server), &filter(100)).await.unwrap();

        assert_eq!(snapshot.merge_requests.len(), 1);
        assert!(snapshot.partial);
    }

    /// A null root is what a group query returns for a path that does not exist or is
    /// not visible to the token. An empty list would hide that.
    #[tokio::test]
    async fn a_null_root_is_an_error_not_an_empty_list() {
        let (server, _, _) = serving(vec![json!({"data": {"group": null}})]).await;

        let err = fresh(&client_for(&server), &filter(100)).await.unwrap_err();
        assert!(err.to_string().contains("scope and path"), "{err}");
    }

    /// A server that promises more pages without advancing the cursor would otherwise
    /// spin until the process is killed.
    #[tokio::test]
    async fn a_stuck_cursor_stops_pagination() {
        let stuck = page(&["1"], true, Some("SAME"));
        let (server, calls, _) = serving(vec![
            stuck.clone(),
            stuck.clone(),
            stuck.clone(),
            stuck.clone(),
        ])
        .await;

        let snapshot = fresh(&client_for(&server), &filter(1000)).await.unwrap();

        assert!(calls.load(Ordering::SeqCst) <= 3, "stopped quickly");
        assert!(!snapshot.merge_requests.is_empty());
    }

    #[tokio::test]
    async fn more_pages_without_a_cursor_stops_pagination() {
        let (server, calls, _) = serving(vec![page(&["1"], true, None)]).await;

        let snapshot = fresh(&client_for(&server), &filter(1000)).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(snapshot.merge_requests.len(), 1);
    }

    #[tokio::test]
    async fn transport_failures_propagate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let err = fresh(&client_for(&server), &filter(100)).await.unwrap_err();
        assert!(matches!(err, Error::Http { status: 503 }), "{err:?}");
    }

    #[tokio::test]
    async fn derived_flags_use_the_current_user() {
        let mut assigned = node("1");
        assigned["assignees"] = json!({"nodes": [{"username": "me"}]});
        let body = json!({"data": {"currentUser": {"assignedMergeRequests": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [assigned]
        }}}});
        let (server, _, _) = serving(vec![body]).await;

        let snapshot = fresh(&client_for(&server), &filter(100)).await.unwrap();

        assert!(snapshot.merge_requests[0].assigned_to_me());
    }
}
