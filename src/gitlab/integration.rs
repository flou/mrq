//! The fetch path end to end, against recorded responses.
//!
//! `fetch` → `client` → `wire` → `error` are each unit-tested against values
//! built in code; this drives the four of them together against bytes a real instance
//! actually sent, over a local HTTP server. The seam that needs it is the one no unit test
//! covers: what the pieces do with a response none of them individually expected.
//!
//! # Nothing here reaches a network or needs a token
//!
//! Every response is a file under `tests/fixtures`, served by `wiremock` on loopback, and
//! the token is a literal. [`every_client_talks_only_to_the_mock`] pins that.
//!
//! # Why the HTTP status is in the code and the body is in a file
//!
//! A fixture file is a recorded *body*. The status and headers are properties of the
//! exchange, not of the payload, and wrapping the body in an envelope to carry them would
//! mean the files no longer round-trip as GraphQL responses — which is what makes them
//! reviewable and reusable by the deserialization tests in `wire`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use jiff::Timestamp;
use serde_json::Value;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::config::schema::{Filter, Gitlab, Scope};
use crate::config::token::{TokenEnv, resolve};
use crate::error::Error;
use crate::gitlab::client::Client;
use crate::gitlab::fetch::{Degradation, Snapshot, fetch};
use crate::gitlab::model::{MergeStatus, PipelineStatus};

const INSTANCE: &str = "https://gitlab.example.com";
const ME: &str = "user1";

fn now() -> Timestamp {
    "2026-09-11T12:00:00Z".parse().unwrap()
}

macro_rules! fixture {
    ($name:literal) => {
        serde_json::from_str::<Value>(include_str!(concat!("../../tests/fixtures/", $name)))
            .expect(concat!($name, " should be valid JSON"))
    };
}

fn filter(max_results: usize) -> Filter {
    Filter {
        max_results,
        ..Filter::named("Assigned", Scope::Assigned)
    }
}

fn client_for(server: &MockServer) -> Client {
    let gitlab = Gitlab {
        url: server.uri(),
        token: Some("glpat-fixture".into()),
        timeout_secs: 5,
        ..Gitlab::default()
    };
    let token = resolve(&gitlab, &TokenEnv::default(), None)
        .expect("a literal token resolves")
        .token;
    Client::new(&gitlab, token).expect("the client builds")
}

/// Replies from a scripted list, recording every request body.
///
/// Requests past the end of the script get the last response again, so a test that
/// expects three round trips fails on the assertion about how many it made rather than on
/// an unrelated transport error.
struct Script {
    responses: Vec<ResponseTemplate>,
    bodies: Arc<std::sync::Mutex<Vec<Value>>>,
    calls: Arc<AtomicUsize>,
}

impl Respond for Script {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Ok(body) = serde_json::from_slice::<Value>(&request.body) {
            self.bodies.lock().unwrap().push(body);
        }
        self.responses
            .get(index)
            .or_else(|| self.responses.last())
            .expect("a scripted response")
            .clone()
    }
}

/// A mock instance, plus the request log and call count.
struct Recording {
    server: MockServer,
    bodies: Arc<std::sync::Mutex<Vec<Value>>>,
    calls: Arc<AtomicUsize>,
}

impl Recording {
    async fn serving(responses: Vec<ResponseTemplate>) -> Self {
        let server = MockServer::start().await;
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));

        Mock::given(method("POST"))
            .respond_with(Script {
                responses,
                bodies: Arc::clone(&bodies),
                calls: Arc::clone(&calls),
            })
            .mount(&server)
            .await;

        Self {
            server,
            bodies,
            calls,
        }
    }

    /// One filter's worth of responses, from an undegraded start.
    async fn fetch(&self, max_results: usize) -> crate::error::Result<Snapshot> {
        self.fetch_with(&mut Degradation::none(), max_results).await
    }

    async fn fetch_with(
        &self,
        degradation: &mut Degradation,
        max_results: usize,
    ) -> crate::error::Result<Snapshot> {
        fetch(
            &client_for(&self.server),
            &filter(max_results),
            degradation,
            ME,
            INSTANCE,
            now(),
        )
        .await
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn request(&self, index: usize) -> Value {
        self.bodies.lock().unwrap()[index].clone()
    }
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

/// The whole of a Premium instance's answer, decoded into the domain model.
#[tokio::test]
async fn a_full_response_decodes_every_documented_field() {
    let recording = Recording::serving(vec![ok(fixture!("full_response.json"))]).await;

    let snapshot = recording.fetch(100).await.unwrap();
    let rows = &snapshot.merge_requests;

    assert_eq!(rows.len(), 7);
    assert_eq!(recording.calls(), 1);
    assert!(!snapshot.truncated);
    assert!(!snapshot.partial);
    assert!(
        snapshot.anomalies.is_empty(),
        "nothing a real instance sends should be unrepresentable: {:?}",
        snapshot.anomalies
    );

    let first = &rows[0];
    assert_eq!(first.iid, "3651", "the iid stays a string");
    assert_eq!(first.project_path, "group1/sub/project-1");
    assert_eq!(first.project_name, "project-1", "derived from the path");
    assert!(first.approved);
    assert_eq!(first.reviewers, ["user2", "user3"]);
    assert!(first.assigned_to_me(), "user1 is in assignees");

    // GitLab returns the pipeline as an instance-relative path, not a URL.
    let pipeline = first.pipeline.as_ref().expect("a pipeline");
    assert_eq!(pipeline.status, PipelineStatus::Success);
    assert!(pipeline.url.starts_with(INSTANCE), "{}", pipeline.url);
    assert!(!pipeline.url.contains("//-/"), "{}", pipeline.url);

    let blocked = rows
        .iter()
        .find(|mr| mr.conflicts)
        .expect("a conflicted MR");
    assert_eq!(blocked.merge_status, MergeStatus::CannotBeMerged);
    assert!(blocked.is_blocked());

    assert!(
        rows.iter().any(|mr| mr.pipeline.is_none()),
        "an MR with no pipeline at all is not an error"
    );
    assert!(rows.iter().any(|mr| mr.draft));
}

/// A response with `approved` nulled for every row must still arrive complete: a null
/// scalar costs that row its approval state, not the fetch.
#[tokio::test]
async fn a_response_with_approved_nulled_keeps_every_row() {
    let recording = Recording::serving(vec![ok(fixture!("premium_nulled.json"))]).await;

    let snapshot = recording.fetch(100).await.unwrap();

    assert_eq!(snapshot.merge_requests.len(), 7, "nothing was dropped");
    for mr in &snapshot.merge_requests {
        assert!(!mr.approved, "null decodes to not approved");
        assert!(!mr.approved_by_me());
        assert!(!mr.title.is_empty(), "the rest of the row survived");
    }
}

/// Partial data is rendered rather than discarded, and the tab is marked.
#[tokio::test]
async fn a_partial_response_is_rendered_and_marked() {
    let recording = Recording::serving(vec![ok(fixture!("partial_with_errors.json"))]).await;

    let snapshot = recording.fetch(100).await.unwrap();

    assert_eq!(snapshot.merge_requests.len(), 3, "what arrived is kept");
    assert!(snapshot.partial, "and the tab knows it is incomplete");
}

/// One page per request, each carrying the previous page's cursor, never aliased into
/// one document.
#[tokio::test]
async fn pagination_sends_the_exact_cursor_sequence() {
    let recording = Recording::serving(vec![
        ok(fixture!("paged_1.json")),
        ok(fixture!("paged_2.json")),
        ok(fixture!("paged_3.json")),
    ])
    .await;

    let snapshot = recording.fetch(100).await.unwrap();

    assert_eq!(snapshot.merge_requests.len(), 7);
    assert_eq!(recording.calls(), 3, "one request per page");
    assert!(!snapshot.truncated, "the server ran out, we did not stop");

    assert!(
        recording.request(0)["variables"].get("after").is_none(),
        "the first page asks for no cursor"
    );
    assert_eq!(recording.request(1)["variables"]["after"], "CURSOR_PAGE_1");
    assert_eq!(recording.request(2)["variables"]["after"], "CURSOR_PAGE_2");

    // Each request is one page, not several aliased into one document.
    for index in 0..3 {
        let query = recording.request(index)["query"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            query.matches("assignedMergeRequests").count(),
            1,
            "request {index} selected the connection more than once:\n{query}"
        );
    }
}

/// `max_results` stops the walk, and the result says so rather than
/// implying the list is complete.
#[tokio::test]
async fn max_results_stops_the_walk_and_marks_the_list_capped() {
    let recording = Recording::serving(vec![
        ok(fixture!("paged_1.json")),
        ok(fixture!("paged_2.json")),
        ok(fixture!("paged_3.json")),
    ])
    .await;

    let snapshot = recording.fetch(4).await.unwrap();

    assert_eq!(snapshot.merge_requests.len(), 4);
    assert!(snapshot.truncated);
    assert_eq!(recording.calls(), 2, "the third page was never asked for");
}

/// A complexity rejection must leave a degraded-but-populated table, not an
/// empty one.
#[tokio::test]
async fn a_complexity_rejection_still_produces_a_populated_table() {
    let recording = Recording::serving(vec![
        ok(fixture!("complexity_rejected.json")),
        ok(fixture!("full_response.json")),
    ])
    .await;

    let mut degradation = Degradation::none();
    let snapshot = recording.fetch_with(&mut degradation, 100).await.unwrap();

    assert_eq!(snapshot.merge_requests.len(), 7, "rows, not an empty tab");
    assert_eq!(recording.calls(), 2, "one rejection, one retry");
    assert!(degradation.is_degraded());

    let asked = |index: usize| {
        recording.request(index)["variables"]["first"]
            .as_u64()
            .unwrap()
    };
    assert!(
        asked(1) < asked(0),
        "the retry asked for a smaller page: {} then {}",
        asked(0),
        asked(1)
    );
}

/// Unknown fields are dropped before the cost ladder is walked, and only
/// the ones the instance named.
#[tokio::test]
async fn an_unknown_field_rejection_drops_only_the_fields_it_names() {
    let recording = Recording::serving(vec![
        ok(fixture!("unknown_field_rejected.json")),
        ok(fixture!("premium_nulled.json")),
    ])
    .await;

    let snapshot = recording.fetch(100).await.unwrap();

    assert_eq!(snapshot.merge_requests.len(), 7);
    assert_eq!(recording.calls(), 2);
    assert!(snapshot.fragment.excludes("userNotesCount"));
    assert!(snapshot.fragment.excludes("resolvableDiscussionsCount"));

    let retried = recording.request(1)["query"].as_str().unwrap().to_owned();
    assert!(!retried.contains("userNotesCount"), "{retried}");
    assert!(
        retried.contains("labels(first:") && retried.contains("reviewers(first:"),
        "two unrelated missing fields must not cost us labels and reviewers:\n{retried}"
    );
}

/// The server's own `Retry-After` beats our backoff, which knows nothing
/// about how long the limit lasts.
#[tokio::test]
async fn a_rate_limited_response_surfaces_its_retry_after() {
    let recording = Recording::serving(vec![
        ResponseTemplate::new(429)
            .insert_header("retry-after", "45")
            .set_body_string(include_str!("../../tests/fixtures/rate_limited.html")),
    ])
    .await;

    let error = recording.fetch(100).await.unwrap_err();

    assert!(
        matches!(error, Error::RateLimited { .. }),
        "a 429 body is HTML, not GraphQL, and must not be read as a parse failure: {error:?}"
    );
    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(45))
    );
}

#[tokio::test]
async fn a_server_error_is_classified_from_its_status_not_its_body() {
    let recording = Recording::serving(vec![
        ResponseTemplate::new(502)
            .set_body_string(include_str!("../../tests/fixtures/server_error.html")),
    ])
    .await;

    let error = recording.fetch(100).await.unwrap_err();

    assert!(matches!(error, Error::Http { status: 502 }), "{error:?}");
    assert_eq!(error.retry_after(), None, "no header, no instruction");
}

/// A transient failure must not be retried into a different shape: the next refresh asks
/// for exactly what the last successful one did.
#[tokio::test]
async fn a_transport_failure_leaves_the_settled_query_shape_alone() {
    let recording = Recording::serving(vec![
        ok(fixture!("complexity_rejected.json")),
        ok(fixture!("full_response.json")),
        ResponseTemplate::new(502),
        ok(fixture!("full_response.json")),
    ])
    .await;

    let mut degradation = Degradation::none();
    recording.fetch_with(&mut degradation, 100).await.unwrap();
    let settled = degradation.clone();

    recording
        .fetch_with(&mut degradation, 100)
        .await
        .unwrap_err();
    assert_eq!(degradation, settled, "a 502 says nothing about the shape");

    recording.fetch_with(&mut degradation, 100).await.unwrap();
    assert_eq!(degradation, settled);
    assert_eq!(
        recording.request(1)["variables"]["first"],
        recording.request(3)["variables"]["first"],
        "the refresh after the failure asked for the shape that worked"
    );
}

/// No test may reach a live instance or need a real token.
///
/// Asserted rather than assumed, because the failure is invisible on a developer machine
/// that happens to have both — it only shows up as a CI job that cannot run offline.
#[tokio::test]
async fn every_client_talks_only_to_the_mock() {
    let server = MockServer::start().await;
    let endpoint = client_for(&server).endpoint().to_owned();

    assert!(
        endpoint.starts_with("http://127.0.0.1:") || endpoint.starts_with("http://localhost:"),
        "the fetch path must only ever reach loopback: {endpoint}"
    );
    assert!(endpoint.ends_with("/api/graphql"), "{endpoint}");

    // `TokenEnv::default()` rather than `from_process`: the suite must not pick up a real
    // token from the developer's environment and must not need one to be present.
    assert!(
        resolve(&Gitlab::default(), &TokenEnv::default(), None).is_err(),
        "with no configured token and an empty environment there is nothing to resolve"
    );
}
