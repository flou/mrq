//! Deserialization: GitLab's JSON to the domain model.
//!
//! The wire types live here rather than on the domain model because the two shapes
//! genuinely differ. GitLab nests every list behind `{ nodes: [...] }`, types `iid` as a
//! string, returns pipeline paths relative to the instance, and omits whole fields on
//! tiers that lack them. Encoding that on [`crate::gitlab::model`] would push GitLab's
//! quirks into the sorter and the renderer.
//!
//! # Nothing here may fail on unexpected data
//!
//! Every field an instance can legitimately omit or extend is `Option` or falls back.
//! One merge request with a pipeline status this build has never heard of must not empty
//! the tab — GitLab adds enum members between releases, and a hard parse failure turns a
//! cosmetic gap into a total outage of the view.
//!
//! The shapes here were taken from a real instance, not from the schema documentation;
//! `tests/fixtures/assigned_mrs.json` is the reduced response they were derived from.

use serde::Deserialize;

use crate::gitlab::model::{
    Label, MergeRequest, MergeStatus, MrState, Pipeline, PipelineStatus, User, absolute_url,
};

/// GitLab wraps every list in a `nodes` array.
#[derive(Debug, Clone, Deserialize)]
pub struct Connection<T> {
    #[serde(default = "Vec::new")]
    pub nodes: Vec<T>,
}

impl<T> Default for Connection<T> {
    fn default() -> Self {
        Self { nodes: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    #[serde(default)]
    pub has_next_page: bool,
    #[serde(default)]
    pub end_cursor: Option<String>,
}

/// A paginated merge-request connection, as every scope returns it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeRequestConnection {
    #[serde(default)]
    pub page_info: PageInfo,
    #[serde(default = "Vec::new")]
    pub nodes: Vec<WireMergeRequest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireUser {
    pub username: String,
    #[serde(default)]
    pub name: Option<String>,
}

impl From<WireUser> for User {
    fn from(u: WireUser) -> Self {
        Self {
            username: u.username,
            name: u.name,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireLabel {
    pub title: String,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireProject {
    pub full_path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireDiffStats {
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(default)]
    pub file_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WirePipeline {
    /// Instance-relative, e.g. `/group/project/-/pipelines/123`.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireMergeRequest {
    pub id: String,
    /// GraphQL `ID!`, serialised as a string even though it reads as a number.
    pub iid: String,
    pub title: String,
    pub web_url: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub state: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub source_branch: String,
    #[serde(default)]
    pub target_branch: String,
    #[serde(default)]
    pub conflicts: bool,
    #[serde(default)]
    pub merge_status_enum: Option<String>,

    pub author: Option<WireUser>,
    pub project: Option<WireProject>,
    #[serde(default)]
    pub diff_stats_summary: Option<WireDiffStats>,

    #[serde(default)]
    pub approved: Option<bool>,

    #[serde(default)]
    pub approved_by: Connection<WireUser>,
    #[serde(default)]
    pub assignees: Connection<WireUser>,
    #[serde(default)]
    pub reviewers: Connection<WireUser>,
    #[serde(default)]
    pub labels: Connection<WireLabel>,

    #[serde(default)]
    pub resolvable_discussions_count: u32,
    #[serde(default)]
    pub resolved_discussions_count: u32,
    #[serde(default)]
    pub user_notes_count: u32,

    #[serde(default)]
    pub head_pipeline: Option<WirePipeline>,
}

/// What could not be represented faithfully while converting a page.
///
/// Returned rather than logged inline so the caller logs once per refresh instead of
/// once per row: a Free-tier instance would otherwise produce a line per merge request,
/// every five minutes, forever.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Anomalies {
    pub unknown_pipeline_statuses: Vec<String>,
    pub unknown_merge_statuses: Vec<String>,
    pub unparsable_timestamps: Vec<String>,
}

impl Anomalies {
    pub const fn is_empty(&self) -> bool {
        self.unknown_pipeline_statuses.is_empty()
            && self.unknown_merge_statuses.is_empty()
            && self.unparsable_timestamps.is_empty()
    }

    fn note_pipeline(&mut self, raw: &str) {
        if !self.unknown_pipeline_statuses.iter().any(|s| s == raw) {
            self.unknown_pipeline_statuses.push(raw.to_owned());
        }
    }

    fn note_merge_status(&mut self, raw: &str) {
        if !self.unknown_merge_statuses.iter().any(|s| s == raw) {
            self.unknown_merge_statuses.push(raw.to_owned());
        }
    }

    fn note_timestamp(&mut self, raw: &str) {
        if !self.unparsable_timestamps.iter().any(|s| s == raw) {
            self.unparsable_timestamps.push(raw.to_owned());
        }
    }
}

fn parse_state(raw: Option<&str>) -> MrState {
    match raw.unwrap_or("opened").to_ascii_lowercase().as_str() {
        "merged" => MrState::Merged,
        "closed" => MrState::Closed,
        "locked" => MrState::Locked,
        _ => MrState::Opened,
    }
}

/// Parse an RFC 3339 timestamp, falling back to the Unix epoch.
///
/// A timestamp we cannot read costs the AGE column its value for one row. Failing the
/// page instead would cost the user the whole tab, which is a far worse trade for a
/// field that only drives a relative-time string.
fn parse_timestamp(raw: &str, anomalies: &mut Anomalies) -> jiff::Timestamp {
    match raw.parse() {
        Ok(ts) => ts,
        Err(_) => {
            anomalies.note_timestamp(raw);
            jiff::Timestamp::UNIX_EPOCH
        }
    }
}

fn convert_pipeline(
    wire: WirePipeline,
    instance_url: &str,
    anomalies: &mut Anomalies,
) -> Option<Pipeline> {
    // A pipeline with no path cannot be opened, which is the only thing the column is
    // for beyond its glyph; treat it as absent rather than rendering an inert row.
    let path = wire.path?;

    let status = match wire.status.as_deref() {
        Some(raw) => {
            let parsed = PipelineStatus::parse(raw);
            if matches!(parsed, PipelineStatus::Unknown(_)) {
                anomalies.note_pipeline(raw);
            }
            parsed
        }
        None => PipelineStatus::Unknown(String::new()),
    };

    Some(Pipeline {
        url: absolute_url(instance_url, &path),
        status,
        finished_at: wire.finished_at.as_deref().and_then(|raw| raw.parse().ok()),
    })
}

impl WireMergeRequest {
    /// Convert one node, recording anything that could not be represented.
    ///
    /// `instance_url` is needed because GitLab returns pipeline paths relative to it.
    pub fn into_model(
        self,
        instance_url: &str,
        current_user: &str,
        anomalies: &mut Anomalies,
    ) -> MergeRequest {
        let merge_status = match self.merge_status_enum.as_deref() {
            Some(raw) => {
                let parsed = MergeStatus::parse(raw);
                if parsed == MergeStatus::Unknown {
                    anomalies.note_merge_status(raw);
                }
                parsed
            }
            None => MergeStatus::Unknown,
        };

        let project_path = self.project.map(|p| p.full_path).unwrap_or_default();
        let project_name = MergeRequest::project_name_from_path(&project_path).to_owned();
        let diff = self.diff_stats_summary.unwrap_or(WireDiffStats {
            additions: 0,
            deletions: 0,
            file_count: 0,
        });

        let mut mr = MergeRequest {
            id: self.id,
            iid: self.iid,
            project_path,
            project_name,
            title: self.title,
            web_url: self.web_url,
            draft: self.draft,
            state: parse_state(self.state.as_deref()),
            author: self
                .author
                .map(User::from)
                .unwrap_or_else(|| User::new("unknown")),
            created_at: parse_timestamp(&self.created_at, anomalies),
            updated_at: parse_timestamp(&self.updated_at, anomalies),
            source_branch: self.source_branch,
            target_branch: self.target_branch,
            additions: diff.additions,
            deletions: diff.deletions,
            files_changed: diff.file_count,
            // A server that legitimately returns null (rather than omitting the field)
            // is treated as not approved rather than as "unknown": inventing a third
            // state here would give the APRV column something to render that never
            // corresponds to a real GitLab answer.
            approved: self.approved.unwrap_or(false),
            approved_by: self
                .approved_by
                .nodes
                .into_iter()
                .map(|u| u.username)
                .collect(),
            assignees: self.assignees.nodes.into_iter().map(User::from).collect(),
            reviewers: self
                .reviewers
                .nodes
                .into_iter()
                .map(|u| u.username)
                .collect(),
            labels: self
                .labels
                .nodes
                .into_iter()
                .map(|l| Label {
                    title: l.title,
                    color: l.color,
                })
                .collect(),
            // Saturating: GitLab can report more resolved than resolvable while a
            // discussion is being deleted, and an underflow here would panic in release
            // builds' wrapping arithmetic or show 4 billion unresolved threads.
            unresolved_discussions: self
                .resolvable_discussions_count
                .saturating_sub(self.resolved_discussions_count),
            notes_count: self.user_notes_count,
            conflicts: self.conflicts,
            merge_status,
            pipeline: self
                .head_pipeline
                .and_then(|p| convert_pipeline(p, instance_url, anomalies)),
            truncated_lists: false,
            approved_by_me: false,
            assigned_to_me: false,
            authored_by_me: false,
        };

        mr.recompute_derived(current_user);
        mr
    }
}

/// One page of merge requests, converted.
#[derive(Debug, Clone)]
pub struct Page {
    pub merge_requests: Vec<MergeRequest>,
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
    pub anomalies: Anomalies,
}

impl MergeRequestConnection {
    pub fn into_page(self, instance_url: &str, current_user: &str) -> Page {
        let mut anomalies = Anomalies::default();
        let merge_requests = self
            .nodes
            .into_iter()
            .map(|n| n.into_model(instance_url, current_user, &mut anomalies))
            .collect();

        Page {
            merge_requests,
            has_next_page: self.page_info.has_next_page,
            end_cursor: self.page_info.end_cursor,
            anomalies,
        }
    }
}

// --- response envelopes, one per query root ---

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentUserScopes {
    #[serde(default)]
    pub assigned_merge_requests: Option<MergeRequestConnection>,
    #[serde(default)]
    pub review_requested_merge_requests: Option<MergeRequestConnection>,
    #[serde(default)]
    pub authored_merge_requests: Option<MergeRequestConnection>,
}

impl CurrentUserScopes {
    /// Whichever connection the query selected.
    pub fn connection(self) -> Option<MergeRequestConnection> {
        self.assigned_merge_requests
            .or(self.review_requested_merge_requests)
            .or(self.authored_merge_requests)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceScope {
    #[serde(default)]
    pub merge_requests: Option<MergeRequestConnection>,
}

/// The `data` object for any scope.
///
/// One type rather than one per scope: they differ only in which key is populated, and a
/// single envelope keeps the caller from having to know which builder produced the
/// document it is decoding.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterData {
    #[serde(default)]
    pub current_user: Option<CurrentUserScopes>,
    #[serde(default)]
    pub group: Option<NamespaceScope>,
    #[serde(default)]
    pub project: Option<NamespaceScope>,
    /// The instance scope's bare `mergeRequests` root: no wrapping object, so it lands
    /// directly on `data` rather than behind `group`/`project`/`currentUser`.
    #[serde(default)]
    pub merge_requests: Option<MergeRequestConnection>,
}

impl FilterData {
    pub fn connection(self) -> Option<MergeRequestConnection> {
        if let Some(user) = self.current_user
            && let Some(connection) = user.connection()
        {
            return Some(connection);
        }
        self.group
            .and_then(|g| g.merge_requests)
            .or_else(|| self.project.and_then(|p| p.merge_requests))
            .or(self.merge_requests)
    }
}

/// The startup identity probe.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentUserData {
    pub current_user: Option<WireUser>,
    #[serde(default)]
    pub metadata: Option<Metadata>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTANCE: &str = "https://gitlab.example.com";
    const ME: &str = "user1";

    fn fixture() -> serde_json::Value {
        let raw = include_str!("../../tests/fixtures/assigned_mrs.json");
        serde_json::from_str(raw).expect("fixture should be valid JSON")
    }

    fn page() -> Page {
        let data: FilterData =
            serde_json::from_value(fixture()["data"].clone()).expect("fixture should deserialize");
        data.connection()
            .expect("fixture has an assigned connection")
            .into_page(INSTANCE, ME)
    }

    /// The fixture is a reduced real response, so this is the closest thing to an
    /// end-to-end decode without a live instance.
    #[test]
    fn the_real_response_decodes_completely() {
        let page = page();

        assert_eq!(page.merge_requests.len(), 11);
        assert!(page.has_next_page);
        assert_eq!(page.end_cursor.as_deref(), Some("OPAQUE_CURSOR"));
        assert!(
            page.anomalies.is_empty(),
            "nothing in a real response should be unrepresentable: {:?}",
            page.anomalies
        );
    }

    #[test]
    fn scalar_fields_survive_the_conversion() {
        let page = page();
        let mr = &page.merge_requests[0];

        assert!(mr.id.starts_with("gid://gitlab/MergeRequest/"));
        assert_eq!(mr.iid, "3658", "iid stays a string");
        assert_eq!(mr.project_path, "group1/sub/project-1");
        assert_eq!(mr.project_name, "project-1", "derived from the path");
        assert_eq!(mr.target_branch, "main");
        assert!(mr.web_url.starts_with(INSTANCE));
        assert_eq!(mr.state, MrState::Opened);
    }

    /// The bug that motivated the fixture: GitLab returns the path, not a URL.
    #[test]
    fn pipeline_paths_become_absolute_urls() {
        let page = page();
        let with_pipeline = page
            .merge_requests
            .iter()
            .find(|m| m.pipeline.is_some())
            .expect("the fixture has pipelines");

        let url = &with_pipeline.pipeline.as_ref().unwrap().url;
        assert!(url.starts_with("https://gitlab.example.com/"), "{url}");
        assert!(url.contains("/-/pipelines/"), "{url}");
        assert!(!url.contains("//-/"), "no doubled separator: {url}");
    }

    /// One merge request in the fixture has no pipeline at all.
    #[test]
    fn a_missing_pipeline_is_absent_not_an_error() {
        let page = page();
        assert!(
            page.merge_requests.iter().any(|m| m.pipeline.is_none()),
            "the fixture should contain an MR with no pipeline"
        );
    }

    /// A MANUAL pipeline has not finished, so `finishedAt` is null.
    #[test]
    fn a_null_finished_at_is_tolerated() {
        let page = page();
        let unfinished = page
            .merge_requests
            .iter()
            .filter_map(|m| m.pipeline.as_ref())
            .find(|p| p.finished_at.is_none());
        assert!(
            unfinished.is_some(),
            "the fixture has an unfinished pipeline"
        );
    }

    #[test]
    fn every_pipeline_status_in_the_fixture_is_recognised() {
        let page = page();
        let statuses: Vec<&PipelineStatus> = page
            .merge_requests
            .iter()
            .filter_map(|m| m.pipeline.as_ref().map(|p| &p.status))
            .collect();

        assert!(statuses.contains(&&PipelineStatus::Success));
        assert!(statuses.contains(&&PipelineStatus::Failed));
        assert!(statuses.contains(&&PipelineStatus::Canceled));
        assert!(statuses.contains(&&PipelineStatus::Manual));
        assert!(
            !statuses
                .iter()
                .any(|s| matches!(s, PipelineStatus::Unknown(_))),
            "a real response should not contain unknown statuses"
        );
    }

    #[test]
    fn every_merge_status_in_the_fixture_is_recognised() {
        let page = page();
        let statuses: Vec<MergeStatus> =
            page.merge_requests.iter().map(|m| m.merge_status).collect();

        assert!(statuses.contains(&MergeStatus::CanBeMerged));
        assert!(statuses.contains(&MergeStatus::CannotBeMerged));
        assert!(statuses.contains(&MergeStatus::Unchecked));
        assert!(
            !statuses.contains(&MergeStatus::Unknown),
            "CANNOT_BE_MERGED_RECHECK must not fall through to Unknown"
        );
    }

    #[test]
    fn connections_flatten_to_usernames_and_labels() {
        let page = page();

        let approved = page
            .merge_requests
            .iter()
            .find(|m| !m.approved_by.is_empty())
            .expect("the fixture has an approved MR");
        assert!(approved.approved_by.iter().all(|u| u.starts_with("user")));

        let labelled = page
            .merge_requests
            .iter()
            .find(|m| !m.labels.is_empty())
            .expect("the fixture has a labelled MR");
        assert!(
            labelled.labels[0]
                .color
                .as_deref()
                .unwrap()
                .starts_with('#')
        );

        assert!(
            page.merge_requests.iter().any(|m| m.reviewers.is_empty()),
            "empty connections decode to empty vectors, not errors"
        );
    }

    #[test]
    fn derived_flags_are_computed_during_conversion() {
        let page = page();
        assert!(
            page.merge_requests.iter().all(|m| m.assigned_to_me()),
            "every MR in an assigned-scope response is assigned to the current user"
        );
    }

    /// The count is a difference, and GitLab can briefly report more
    /// resolved than resolvable while a discussion is being deleted.
    #[test]
    fn unresolved_discussions_saturate_at_zero() {
        let mut wire = one_node();
        wire.resolvable_discussions_count = 1;
        wire.resolved_discussions_count = 5;

        let mut anomalies = Anomalies::default();
        let mr = wire.into_model(INSTANCE, ME, &mut anomalies);
        assert_eq!(mr.unresolved_discussions, 0, "must not underflow");
    }

    #[test]
    fn unresolved_discussions_are_the_difference() {
        let mut wire = one_node();
        wire.resolvable_discussions_count = 5;
        wire.resolved_discussions_count = 2;

        let mut anomalies = Anomalies::default();
        assert_eq!(
            wire.into_model(INSTANCE, ME, &mut anomalies)
                .unresolved_discussions,
            3
        );
    }

    fn one_node() -> WireMergeRequest {
        let value = fixture()["data"]["currentUser"]["assignedMergeRequests"]["nodes"][0].clone();
        serde_json::from_value(value).unwrap()
    }

    /// `approved` is `Boolean`, not `Boolean!`, so a server can legitimately answer null.
    #[test]
    fn a_null_approved_field_decodes_to_not_approved() {
        let mut value =
            fixture()["data"]["currentUser"]["assignedMergeRequests"]["nodes"][0].clone();
        value["approved"] = serde_json::Value::Null;

        let wire: WireMergeRequest = serde_json::from_value(value).unwrap();
        let mut anomalies = Anomalies::default();
        let mr = wire.into_model(INSTANCE, ME, &mut anomalies);

        assert!(!mr.approved);
        assert!(anomalies.is_empty(), "a null scalar is not an anomaly");
    }

    /// An instance that omits `approved` entirely must not fail the row.
    #[test]
    fn an_absent_approved_field_decodes_to_not_approved() {
        let mut value =
            fixture()["data"]["currentUser"]["assignedMergeRequests"]["nodes"][0].clone();
        value.as_object_mut().unwrap().remove("approved");

        let wire: WireMergeRequest = serde_json::from_value(value).unwrap();
        let mut anomalies = Anomalies::default();
        assert!(!wire.into_model(INSTANCE, ME, &mut anomalies).approved);
    }

    /// GitLab adds enum members between releases. One unknown status must cost a glyph,
    /// not the whole tab.
    #[test]
    fn an_unknown_pipeline_status_degrades_and_is_recorded_once() {
        let mut value =
            fixture()["data"]["currentUser"]["assignedMergeRequests"]["nodes"][0].clone();
        value["headPipeline"]["status"] = serde_json::json!("QUANTUM_SUPERPOSITION");

        let mut anomalies = Anomalies::default();
        for _ in 0..3 {
            let wire: WireMergeRequest = serde_json::from_value(value.clone()).unwrap();
            let mr = wire.into_model(INSTANCE, ME, &mut anomalies);
            assert_eq!(
                mr.pipeline.unwrap().status,
                PipelineStatus::Unknown("QUANTUM_SUPERPOSITION".into())
            );
        }

        assert_eq!(
            anomalies.unknown_pipeline_statuses,
            ["QUANTUM_SUPERPOSITION"],
            "recorded once however many rows carry it"
        );
    }

    #[test]
    fn an_unknown_merge_status_degrades_and_is_recorded() {
        let mut value =
            fixture()["data"]["currentUser"]["assignedMergeRequests"]["nodes"][0].clone();
        value["mergeStatusEnum"] = serde_json::json!("BRAND_NEW_STATE");

        let wire: WireMergeRequest = serde_json::from_value(value).unwrap();
        let mut anomalies = Anomalies::default();
        let mr = wire.into_model(INSTANCE, ME, &mut anomalies);

        assert_eq!(mr.merge_status, MergeStatus::Unknown);
        assert!(!mr.is_blocked(), "unknown is not treated as a failure");
        assert_eq!(anomalies.unknown_merge_statuses, ["BRAND_NEW_STATE"]);
    }

    /// An unreadable timestamp costs one row its AGE value. Failing the page instead
    /// would cost the user every row.
    #[test]
    fn an_unparsable_timestamp_falls_back_rather_than_failing() {
        let mut value =
            fixture()["data"]["currentUser"]["assignedMergeRequests"]["nodes"][0].clone();
        value["createdAt"] = serde_json::json!("not a date");

        let wire: WireMergeRequest = serde_json::from_value(value).unwrap();
        let mut anomalies = Anomalies::default();
        let mr = wire.into_model(INSTANCE, ME, &mut anomalies);

        assert_eq!(mr.created_at, jiff::Timestamp::UNIX_EPOCH);
        assert_eq!(anomalies.unparsable_timestamps, ["not a date"]);
    }

    #[test]
    fn a_missing_author_or_project_does_not_fail_the_row() {
        let mut value =
            fixture()["data"]["currentUser"]["assignedMergeRequests"]["nodes"][0].clone();
        value["author"] = serde_json::Value::Null;
        value["project"] = serde_json::Value::Null;
        value["diffStatsSummary"] = serde_json::Value::Null;

        let wire: WireMergeRequest = serde_json::from_value(value).unwrap();
        let mut anomalies = Anomalies::default();
        let mr = wire.into_model(INSTANCE, ME, &mut anomalies);

        assert_eq!(mr.author.username, "unknown");
        assert_eq!(mr.project_path, "");
        assert_eq!(mr.additions, 0);
    }

    /// Every scope decodes through one envelope, whichever root ends up populated.
    #[test]
    fn every_scope_root_decodes_through_one_envelope() {
        let node = fixture()["data"]["currentUser"]["assignedMergeRequests"]["nodes"][0].clone();
        let connection = serde_json::json!({
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [node]
        });

        let roots = [
            serde_json::json!({"currentUser": {"assignedMergeRequests": connection}}),
            serde_json::json!({"currentUser": {"reviewRequestedMergeRequests": connection}}),
            serde_json::json!({"currentUser": {"authoredMergeRequests": connection}}),
            serde_json::json!({"group": {"mergeRequests": connection}}),
            serde_json::json!({"project": {"mergeRequests": connection}}),
            serde_json::json!({"mergeRequests": connection}),
        ];

        for root in roots {
            let data: FilterData = serde_json::from_value(root.clone()).unwrap();
            let page = data
                .connection()
                .unwrap_or_else(|| panic!("no connection found in {root}"))
                .into_page(INSTANCE, ME);
            assert_eq!(page.merge_requests.len(), 1);
            assert!(!page.has_next_page);
        }
    }

    #[test]
    fn an_empty_result_set_is_an_empty_page_not_an_error() {
        let data: FilterData = serde_json::from_value(serde_json::json!({
            "currentUser": {"assignedMergeRequests": {"pageInfo": {"hasNextPage": false}, "nodes": []}}
        }))
        .unwrap();

        let page = data.connection().unwrap().into_page(INSTANCE, ME);
        assert!(page.merge_requests.is_empty());
        assert!(page.end_cursor.is_none());
    }

    /// A null connection is what a group query returns when the path does not exist or
    /// is not visible to the token.
    #[test]
    fn a_null_root_yields_no_connection() {
        let data: FilterData = serde_json::from_value(serde_json::json!({"group": null})).unwrap();
        assert!(data.connection().is_none());
    }

    #[test]
    fn the_identity_probe_tolerates_absent_metadata() {
        let data: CurrentUserData = serde_json::from_value(serde_json::json!({
            "currentUser": {"username": "someone"}
        }))
        .unwrap();

        assert_eq!(data.current_user.unwrap().username, "someone");
        assert!(data.metadata.is_none());
    }
}
