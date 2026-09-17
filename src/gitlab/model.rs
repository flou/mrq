//! The domain model.
//!
//! Deliberately not the GraphQL shape: GitLab nests lists behind `{ nodes: [...] }` and
//! types `iid` as a string, and pushing that into the sorter and renderer would spread
//! GitLab's quirks through the rest of the program.
//!
//! These types are the contract between the data layer, the sorting and diffing logic,
//! and the renderer, so they are plain data: no methods that fetch, and no reference to
//! the token or the HTTP client. They are `Serialize` so a snapshot can be cached
//!; the token type deliberately has no `Serialize` impl, which is what
//! keeps it from ever reaching a cache file through here.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// A GitLab user, reduced to what the table and sidebar render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    /// Display name. Absent on some instances for service accounts.
    pub name: Option<String>,
}

impl User {
    pub fn new(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            name: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub title: String,
    /// Hex colour as GitLab reports it, e.g. `#428BCA`. The theme decides whether to use
    /// it — a 16-colour terminal cannot, and a dark label on a dark background is worse
    /// than no colour at all.
    pub color: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MrState {
    Opened,
    Merged,
    Closed,
    Locked,
}

/// Whether GitLab thinks the merge request can be merged right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    CanBeMerged,
    CannotBeMerged,
    Checking,
    Unchecked,
    /// An enum member this build does not know. Kept rather than rejected, for the same
    /// reason as [`PipelineStatus::Unknown`].
    Unknown,
}

impl MrState {
    /// How the state reads in the sidebar.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Opened => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Locked => "locked",
        }
    }
}

impl MergeStatus {
    /// Parse GitLab's `MergeStatus` enum, which is SCREAMING_SNAKE_CASE on the wire
    /// (`CAN_BE_MERGED`), not the snake_case the serde attribute implies.
    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_uppercase().as_str() {
            "CAN_BE_MERGED" => Self::CanBeMerged,
            "CANNOT_BE_MERGED" | "CANNOT_BE_MERGED_RECHECK" => Self::CannotBeMerged,
            "CHECKING" | "PREPARING" => Self::Checking,
            "UNCHECKED" => Self::Unchecked,
            _ => Self::Unknown,
        }
    }

    /// Whether the title should be rendered in the warning colour.
    pub const fn is_blocked(self) -> bool {
        matches!(self, Self::CannotBeMerged)
    }
}

/// A CI pipeline status.
///
/// [`PipelineStatus::Unknown`] carries the original string rather than being dropped:
/// GitLab adds statuses over time, and a new one must degrade to a neutral glyph, not
/// fail the whole fetch and leave the user with an empty tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus {
    Created,
    WaitingForResource,
    Preparing,
    Pending,
    Running,
    Success,
    Failed,
    Canceled,
    Skipped,
    Manual,
    Scheduled,
    Unknown(String),
}

impl PipelineStatus {
    /// Parse GitLab's `PipelineStatusEnum`, which is SCREAMING_SNAKE_CASE.
    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_uppercase().as_str() {
            "CREATED" => Self::Created,
            "WAITING_FOR_RESOURCE" => Self::WaitingForResource,
            "PREPARING" => Self::Preparing,
            "PENDING" => Self::Pending,
            "RUNNING" => Self::Running,
            "SUCCESS" => Self::Success,
            "FAILED" => Self::Failed,
            "CANCELED" | "CANCELLED" | "CANCELING" | "CANCELLING" => Self::Canceled,
            "SKIPPED" => Self::Skipped,
            "MANUAL" => Self::Manual,
            "SCHEDULED" => Self::Scheduled,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Whether the pipeline is still going to change on its own.
    #[cfg(test)]
    const fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Created
                | Self::WaitingForResource
                | Self::Preparing
                | Self::Pending
                | Self::Running
        )
    }

    /// How the status reads in prose, for notification messages.
    ///
    /// On the enum rather than at the call site for the same reason `severity` is: adding
    /// a status should force a decision here rather than silently render as `Unknown`.
    pub fn label(&self) -> &str {
        match self {
            Self::Created => "created",
            Self::WaitingForResource => "waiting for a resource",
            Self::Preparing => "preparing",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "passed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Skipped => "skipped",
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
            Self::Unknown(raw) => raw,
        }
    }

    /// Sort severity, lowest first: failed, running, pending, manual, canceled,
    /// skipped, success, none.
    ///
    /// It lives on the enum rather than in the sort module so that adding a status
    /// forces a decision here instead of silently landing at the end of the list.
    pub const fn severity(&self) -> u8 {
        match self {
            Self::Failed => 0,
            Self::Running => 1,
            Self::Created | Self::WaitingForResource | Self::Preparing | Self::Pending => 2,
            Self::Manual | Self::Scheduled => 3,
            Self::Canceled => 4,
            Self::Skipped => 5,
            Self::Success => 6,
            Self::Unknown(_) => 7,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pipeline {
    /// Absolute URL, ready to hand to a browser.
    ///
    /// GitLab returns `headPipeline.path` as an instance-relative path
    /// (`/group/project/-/pipelines/123`), so deserialization joins it onto the
    /// configured instance URL. Storing the raw path would put the join at every call
    /// site and make `p` open a broken link the first time one was forgotten.
    pub url: String,
    pub status: PipelineStatus,
    pub finished_at: Option<Timestamp>,
}

/// Join an instance-relative path from GitLab onto the configured instance URL.
///
/// Values that are already absolute pass through, since GitLab is inconsistent about
/// which fields are paths and which are URLs.
pub fn absolute_url(instance_url: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_owned();
    }
    format!(
        "{}/{}",
        instance_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// A merge request, as `mrq` renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRequest {
    /// GitLab's global id. Stable across refreshes, which is what selection tracking and
    /// snapshot diffing key on — `iid` is only unique per project.
    pub id: String,
    /// The per-project number shown as `!3658`.
    ///
    /// A `String`, not an integer: GraphQL types it as `ID!` and GitLab serialises it as
    /// `"3658"`. It is only ever displayed and put in URLs, so parsing it would buy
    /// nothing and would fail the whole fetch on any instance that returns something
    /// unexpected.
    pub iid: String,
    pub project_path: String,
    pub project_name: String,
    pub title: String,
    pub web_url: String,
    pub draft: bool,
    pub state: MrState,
    pub author: User,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub source_branch: String,
    pub target_branch: String,

    pub additions: u32,
    pub deletions: u32,
    pub files_changed: u32,

    /// Whether the merge request has all the approvals it needs, per GitLab's own
    /// resolution — trivially `true` when no approval rule applies.
    pub approved: bool,
    pub approved_by: Vec<String>,

    pub assignees: Vec<String>,
    pub reviewers: Vec<String>,
    pub labels: Vec<Label>,

    /// `resolvable - resolved`, saturating.
    pub unresolved_discussions: u32,
    pub notes_count: u32,
    pub conflicts: bool,
    pub merge_status: MergeStatus,
    pub pipeline: Option<Pipeline>,

    /// Set to `true` when a capped nested connection had more entries than we asked for,
    /// so the UI can render `+N` honestly instead of implying the list is complete.
    #[serde(default)]
    pub truncated_lists: bool,

    // Derived, see `recompute_derived`. Visible to the data layer so it can construct a
    // value, and to nothing else: outside `gitlab` these are readable only through the
    // accessors, so no caller can set one without the recompute that keeps it true.
    #[serde(default)]
    pub(super) approved_by_me: bool,
    #[serde(default)]
    pub(super) assigned_to_me: bool,
    #[serde(default)]
    pub(super) authored_by_me: bool,
}

impl MergeRequest {
    /// Recompute the per-user flags against the current username.
    ///
    /// Computed once per refresh, not per frame: the table re-renders many times per
    /// second and each of these is a linear scan of a `Vec<String>`.
    ///
    /// Called after deserialization too — a cached snapshot may have been written by a
    /// different account, and stale flags would make the ASSIGNED column lie.
    pub fn recompute_derived(&mut self, current_user: &str) {
        self.approved_by_me = contains_user(&self.approved_by, current_user);
        self.assigned_to_me = contains_user(&self.assignees, current_user);
        self.authored_by_me = eq_user(&self.author.username, current_user);
    }

    /// Re-derive the flags once the identity probe lands, reporting whether any of them
    /// changed.
    ///
    /// The change bit is what the caller redraws on: a cache written by this same
    /// account — overwhelmingly the common case — re-derives to exactly what is already
    /// on screen, and repainting for that is a frame nobody asked for.
    pub fn rederive(&mut self, current_user: &str) -> bool {
        let before = (
            self.approved_by_me,
            self.assigned_to_me,
            self.authored_by_me,
        );
        self.recompute_derived(current_user);
        before
            != (
                self.approved_by_me,
                self.assigned_to_me,
                self.authored_by_me,
            )
    }

    #[cfg(test)]
    pub const fn approved_by_me(&self) -> bool {
        self.approved_by_me
    }

    pub const fn assigned_to_me(&self) -> bool {
        self.assigned_to_me
    }

    pub const fn authored_by_me(&self) -> bool {
        self.authored_by_me
    }

    /// Total lines touched, for the DIFF sort.
    pub fn diff_size(&self) -> u64 {
        u64::from(self.additions) + u64::from(self.deletions)
    }

    /// Whether the title should be rendered as a problem.
    pub const fn is_blocked(&self) -> bool {
        self.conflicts || self.merge_status.is_blocked()
    }

    /// The short project name for the REPO column.
    ///
    /// Derived from `fullPath`, not from GitLab's `project.name`. The latter is a human
    /// display name — "Argocd GCP Values Platform" for a project whose path is
    /// `argocd-gcp-values` — which is both wrong for the column and far too wide for it.
    pub fn project_name_from_path(full_path: &str) -> &str {
        full_path.rsplit('/').next().unwrap_or(full_path)
    }
}

/// GitLab usernames are case-insensitive in practice, and the casing GraphQL returns for
/// `currentUser` does not always match what appears in an assignee list.
const fn eq_user(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn contains_user(haystack: &[String], needle: &str) -> bool {
    haystack.iter().any(|u| eq_user(u, needle))
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A merge request with plausible values, for tests to mutate.
    pub fn mr(id: &str, author: &str) -> MergeRequest {
        MergeRequest {
            id: id.to_owned(),
            iid: "482".to_owned(),
            project_path: "acme/web/web-app".to_owned(),
            project_name: "web-app".to_owned(),
            title: "Add dark mode toggle".to_owned(),
            web_url: format!(
                "https://gitlab.example.com/acme/web/web-app/-/merge_requests/482/{id}"
            ),
            draft: false,
            state: MrState::Opened,
            author: User::new(author),
            created_at: "2026-09-10T09:00:00Z".parse().unwrap(),
            updated_at: "2026-09-11T08:30:00Z".parse().unwrap(),
            source_branch: "feat/dark".to_owned(),
            target_branch: "main".to_owned(),
            additions: 310,
            deletions: 4,
            files_changed: 9,
            approved: false,
            approved_by: vec!["jdoe".to_owned()],
            assignees: vec!["asmith".to_owned()],
            reviewers: vec!["jdoe".to_owned(), "bwayne".to_owned()],
            labels: vec![Label {
                title: "frontend".to_owned(),
                color: Some("#428BCA".to_owned()),
            }],
            unresolved_discussions: 2,
            notes_count: 7,
            conflicts: false,
            merge_status: MergeStatus::CanBeMerged,
            pipeline: Some(Pipeline {
                url: "https://gitlab.example.com/acme/web/web-app/-/pipelines/99".to_owned(),
                status: PipelineStatus::Running,
                finished_at: None,
            }),
            truncated_lists: false,
            approved_by_me: false,
            assigned_to_me: false,
            authored_by_me: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::mr;
    use super::*;

    #[test]
    fn derived_flags_are_computed_against_the_current_user() {
        let mut m = mr("gid://gitlab/MergeRequest/1", "asmith");
        m.recompute_derived("asmith");
        assert!(m.authored_by_me());
        assert!(m.assigned_to_me(), "asmith is in assignees");
        assert!(!m.approved_by_me(), "jdoe approved, not asmith");

        m.recompute_derived("jdoe");
        assert!(!m.authored_by_me());
        assert!(!m.assigned_to_me());
        assert!(m.approved_by_me());

        m.recompute_derived("nobody");
        assert!(!m.authored_by_me() && !m.assigned_to_me() && !m.approved_by_me());
    }

    /// `rederive` is what the identity handler redraws on: a no-op recompute must report
    /// no change, and an actual account switch must report one.
    #[test]
    fn rederive_reports_whether_a_flag_changed() {
        let mut m = mr("1", "asmith");
        m.recompute_derived("asmith");

        assert!(!m.rederive("asmith"), "the same account changes nothing");
        assert!(
            m.rederive("someone-else"),
            "a different account must be reported as a change"
        );
    }

    /// The casing GraphQL returns for `currentUser` does not reliably match the casing in
    /// an assignee list, and a case mismatch would make the ASSIGNED column read "No" for
    /// merge requests that are in fact assigned to you.
    #[test]
    fn username_comparison_ignores_case() {
        let mut m = mr("1", "ASmith");
        m.assignees = vec!["ASMITH".into()];
        m.recompute_derived("asmith");

        assert!(m.authored_by_me());
        assert!(m.assigned_to_me());
    }

    /// A cached snapshot may have been written by a different account; stale flags would
    /// make the ASSIGNED column lie after an account switch.
    #[test]
    fn derived_flags_survive_a_cache_round_trip_only_after_recompute() {
        let mut m = mr("1", "asmith");
        m.recompute_derived("asmith");

        let json = serde_json::to_string(&m).unwrap();
        let mut restored: MergeRequest = serde_json::from_str(&json).unwrap();
        assert!(restored.assigned_to_me(), "flags round-trip as data");

        restored.recompute_derived("someone-else");
        assert!(
            !restored.assigned_to_me(),
            "and are corrected for the current account"
        );
    }

    #[test]
    fn a_cache_entry_without_derived_fields_still_loads() {
        // Forward compatibility with a cache written before the flags existed: they
        // default to false and are corrected by the recompute that follows a load.
        let mut m = mr("1", "asmith");
        m.recompute_derived("asmith");
        let mut value = serde_json::to_value(&m).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("approved_by_me");
        obj.remove("assigned_to_me");
        obj.remove("authored_by_me");

        let restored: MergeRequest = serde_json::from_value(value).unwrap();
        assert!(!restored.assigned_to_me());
    }

    /// A status this build does not know must degrade, not fail the fetch
    /// and leave the tab empty.
    #[test]
    fn an_unrecognised_pipeline_status_is_preserved_not_rejected() {
        let status = PipelineStatus::parse("SOME_FUTURE_STATUS");
        assert_eq!(status, PipelineStatus::Unknown("SOME_FUTURE_STATUS".into()));
        assert!(!status.is_active());
        assert_eq!(status.severity(), 7, "sorts last, after success");
    }

    #[test]
    fn pipeline_statuses_parse_from_gitlabs_enum_casing() {
        assert_eq!(PipelineStatus::parse("SUCCESS"), PipelineStatus::Success);
        assert_eq!(PipelineStatus::parse("success"), PipelineStatus::Success);
        assert_eq!(
            PipelineStatus::parse("WAITING_FOR_RESOURCE"),
            PipelineStatus::WaitingForResource
        );
        // GitLab has used both spellings, and CANCELING appears mid-cancellation.
        for spelling in ["CANCELED", "CANCELLED", "CANCELING"] {
            assert_eq!(PipelineStatus::parse(spelling), PipelineStatus::Canceled);
        }
    }

    /// The documented severity order; the pipeline column sorts by it.
    #[test]
    fn pipeline_severity_matches_the_spec_order() {
        let order = [
            PipelineStatus::Failed,
            PipelineStatus::Running,
            PipelineStatus::Pending,
            PipelineStatus::Manual,
            PipelineStatus::Canceled,
            PipelineStatus::Skipped,
            PipelineStatus::Success,
        ];
        for pair in order.windows(2) {
            assert!(
                pair[0].severity() < pair[1].severity(),
                "{:?} should sort before {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn active_statuses_are_the_ones_still_moving() {
        for s in [
            PipelineStatus::Created,
            PipelineStatus::Pending,
            PipelineStatus::Running,
            PipelineStatus::Preparing,
            PipelineStatus::WaitingForResource,
        ] {
            assert!(s.is_active(), "{s:?}");
        }
        for s in [
            PipelineStatus::Success,
            PipelineStatus::Failed,
            PipelineStatus::Canceled,
            PipelineStatus::Skipped,
            PipelineStatus::Manual,
        ] {
            assert!(!s.is_active(), "{s:?}");
        }
    }

    #[test]
    fn blocked_covers_both_conflicts_and_merge_status() {
        let mut m = mr("1", "asmith");
        assert!(!m.is_blocked());

        m.conflicts = true;
        assert!(m.is_blocked());

        m.conflicts = false;
        m.merge_status = MergeStatus::CannotBeMerged;
        assert!(m.is_blocked());

        m.merge_status = MergeStatus::Checking;
        assert!(!m.is_blocked(), "checking is not a failure");
    }

    #[test]
    fn diff_size_cannot_overflow() {
        let mut m = mr("1", "asmith");
        m.additions = u32::MAX;
        m.deletions = u32::MAX;
        assert_eq!(m.diff_size(), u64::from(u32::MAX) * 2);
    }

    /// GitLab serialises `iid` as a JSON string, not a number. Typing it as an integer
    /// failed deserialization against a real instance.
    #[test]
    fn iid_round_trips_as_a_string() {
        let m = mr("1", "asmith");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains(r#""iid":"482""#), "{json}");

        let restored: MergeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.iid, "482");
    }

    /// `mergeStatusEnum` arrives as `CAN_BE_MERGED`, not `can_be_merged`.
    #[test]
    fn merge_status_parses_gitlabs_enum_casing() {
        assert_eq!(
            MergeStatus::parse("CAN_BE_MERGED"),
            MergeStatus::CanBeMerged
        );
        assert_eq!(
            MergeStatus::parse("CANNOT_BE_MERGED"),
            MergeStatus::CannotBeMerged
        );
        assert_eq!(MergeStatus::parse("CHECKING"), MergeStatus::Checking);
        assert_eq!(MergeStatus::parse("UNCHECKED"), MergeStatus::Unchecked);
        assert_eq!(MergeStatus::parse("SOMETHING_NEW"), MergeStatus::Unknown);

        assert!(MergeStatus::parse("CANNOT_BE_MERGED").is_blocked());
        assert!(!MergeStatus::parse("CAN_BE_MERGED").is_blocked());
        assert!(
            !MergeStatus::parse("CHECKING").is_blocked(),
            "still checking is not a failure"
        );
    }

    /// GitLab's `project.name` is a display name, not the path segment.
    #[test]
    fn project_name_is_the_last_path_segment() {
        assert_eq!(
            MergeRequest::project_name_from_path("my-groups/group-a/my-project"),
            "my-project"
        );
        assert_eq!(MergeRequest::project_name_from_path("solo"), "solo");
        assert_eq!(MergeRequest::project_name_from_path(""), "");
    }

    /// GitLab returns `headPipeline.path` instance-relative, so `p` would open a broken
    /// link without the join.
    #[test]
    fn relative_pipeline_paths_become_absolute() {
        let base = "https://gitlab.example.com";
        assert_eq!(
            absolute_url(base, "/group-a/x/-/pipelines/1262156"),
            "https://gitlab.example.com/group-a/x/-/pipelines/1262156"
        );
        assert_eq!(
            absolute_url("https://gitlab.example.com/", "/group-a/x"),
            "https://gitlab.example.com/group-a/x",
            "no double slash when the instance URL has a trailing one"
        );
        assert_eq!(
            absolute_url(base, "group-a/x"),
            "https://gitlab.example.com/group-a/x",
            "and none missing when the path has no leading slash"
        );
    }

    /// GitLab is inconsistent about paths versus URLs, so an already-absolute value must
    /// pass through rather than being prefixed twice.
    #[test]
    fn absolute_urls_pass_through_unchanged() {
        let url = "https://gitlab.example.com/group-a/x/-/merge_requests/1";
        assert_eq!(absolute_url("https://other.example.com", url), url);
    }

    #[test]
    fn the_model_round_trips_through_the_cache_format() {
        let mut m = mr("gid://gitlab/MergeRequest/482", "asmith");
        m.recompute_derived("asmith");

        let json = serde_json::to_string(&m).unwrap();
        let restored: MergeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, m);
    }

    /// The cache must never be able to carry a credential. The model has no field for
    /// one, and this asserts the serialized form stays that way.
    #[test]
    fn the_serialized_model_contains_no_credential_fields() {
        let m = mr("1", "asmith");
        let json = serde_json::to_string(&m).unwrap().to_lowercase();
        for forbidden in ["token", "authorization", "bearer", "password", "secret"] {
            assert!(!json.contains(forbidden), "`{forbidden}` in cached model");
        }
    }
}
