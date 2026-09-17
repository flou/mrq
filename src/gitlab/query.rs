//! The `MrFields` fragment and the per-scope query builders.
//!
//! Five filter scopes map onto three different GraphQL roots with different accepted
//! arguments, but they all select the same fragment — one selection to reason about
//! instead of five.
//!
//! # Documents are values
//!
//! [`build`] returns a [`Document`] and [`all_documents`] enumerates every one the binary
//! can emit, which is only possible if no query is assembled ad hoc at a call site.
//!
//! # Variables, never interpolation
//!
//! Filter arguments are GraphQL variables. A label containing a quote would otherwise
//! produce a malformed document — and a malformed document returns no data at all, which
//! surfaces as an empty tab rather than as an error anyone can act on.
//!
//! Only the variables actually used are declared: GraphQL rejects a document that
//! declares a variable it never references, so the declaration list is built alongside
//! the argument list rather than being a fixed preamble.

use jiff::{Span, Timestamp};
use serde_json::{Map, Value, json};

use crate::config::schema::{Filter, Scope, StateFilter};

/// The starting page size — one constant the tests read rather than a literal in the
/// builder.
#[cfg(test)]
pub(crate) const DEFAULT_PAGE_SIZE: u32 = 20;

/// The page sizes the runtime fallback can walk down to.
pub const FALLBACK_PAGE_SIZES: [u32; 3] = [20, 10, 5];

/// Caps on nested connections. Unbounded connections are scored
/// against the schema's default page size, which is far larger than the UI renders.
const APPROVED_BY_CAP: u32 = 10;
const ASSIGNEES_CAP: u32 = 5;
const REVIEWERS_CAP: u32 = 5;
const LABELS_CAP: u32 = 10;

/// Which parts of `MrFields` to ask for.
///
/// Two independent reductions, for two unrelated failures:
///
/// - `excluded` drops individual fields the server has rejected as unknown. GraphQL
///   names them precisely in `extensions.fieldName`, so the retry can drop exactly those
///   and keep everything else, rather than guessing at a whole category of "premium"
///   fields to drop together.
/// - `minimal` drops the nested connections wholesale, and is the last rung of the
///   complexity ladder, where the goal is fitting a budget rather than avoiding a field.
///
/// Serialized into the snapshot cache so a warm render of a degraded query
/// still says which fields are missing, rather than showing blanks for a few seconds with
/// no explanation.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fragment {
    excluded: std::collections::BTreeSet<String>,
    minimal: bool,
}

impl Fragment {
    /// Everything.
    pub fn full() -> Self {
        Self::default()
    }

    /// Nested connections dropped, for the last rung of the complexity ladder.
    pub fn minimal() -> Self {
        Self {
            minimal: true,
            ..Self::default()
        }
    }

    /// The same selection with further fields removed.
    #[must_use]
    pub fn excluding<I, S>(&self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut next = self.clone();
        next.excluded.extend(fields.into_iter().map(Into::into));
        next
    }

    pub fn excludes(&self, field: &str) -> bool {
        self.excluded.contains(field)
    }

    pub const fn is_minimal(&self) -> bool {
        self.minimal
    }

    /// Whether anything has been dropped, so the UI can mark the tab as degraded.
    pub fn is_degraded(&self) -> bool {
        self.minimal || !self.excluded.is_empty()
    }

    /// What the user can no longer see, in their terms rather than GraphQL's.
    ///
    /// The status bar has to explain a degraded tab, and "reviewers,
    /// labels" is the answer to that; the field names are only meaningful in the log.
    /// `None` when nothing was dropped.
    pub fn lost(&self) -> Option<String> {
        let mut lost: Vec<&str> = Vec::new();
        if self.minimal {
            // `assignees` survives the reduction, so it is deliberately not listed.
            lost.extend(["reviewers", "labels", "approvers"]);
        }
        for field in &self.excluded {
            let name = field.as_str();
            if !lost.contains(&name) {
                lost.push(name);
            }
        }

        (!lost.is_empty()).then(|| lost.join(", "))
    }

    fn label(&self) -> String {
        match (self.minimal, self.excluded.is_empty()) {
            (true, true) => "minimal".to_owned(),
            (true, false) => format!("minimal-{}", self.excluded.len()),
            (false, true) => "full".to_owned(),
            (false, false) => format!("full-{}", self.excluded.len()),
        }
    }
}

/// A complete GraphQL document plus its variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub query: String,
    pub variables: Value,
    pub page_size: u32,
    pub fragment: Fragment,
    /// Human-readable identity, used by tests to name a failure.
    pub label: String,
}

/// The shared field selection every scope queries.
///
/// Every nested connection carries an explicit `first:`. Changing this selection changes
/// the query cost against GitLab's complexity ceiling, so a new field should be checked
/// against a live instance before it ships.
fn mr_fields(fragment: &Fragment) -> String {
    // (field name, rendered selection, is a nested connection). The name is what an
    // `undefinedField` error reports, so exclusion matches on it directly.
    let selections: Vec<(&str, String, bool)> = vec![
        ("id", "id".into(), false),
        ("iid", "iid".into(), false),
        ("title", "title".into(), false),
        ("webUrl", "webUrl".into(), false),
        ("draft", "draft".into(), false),
        ("state", "state".into(), false),
        ("createdAt", "createdAt".into(), false),
        ("updatedAt", "updatedAt".into(), false),
        ("sourceBranch", "sourceBranch".into(), false),
        ("targetBranch", "targetBranch".into(), false),
        ("conflicts", "conflicts".into(), false),
        ("mergeStatusEnum", "mergeStatusEnum".into(), false),
        ("author", "author { username name }".into(), false),
        ("project", "project { fullPath }".into(), false),
        (
            "diffStatsSummary",
            "diffStatsSummary { additions deletions fileCount }".into(),
            false,
        ),
        (
            "resolvableDiscussionsCount",
            "resolvableDiscussionsCount".into(),
            false,
        ),
        (
            "resolvedDiscussionsCount",
            "resolvedDiscussionsCount".into(),
            false,
        ),
        ("userNotesCount", "userNotesCount".into(), false),
        (
            "headPipeline",
            "headPipeline {\n    id\n    path\n    status\n    detailedStatus { text label }\n    createdAt\n    finishedAt\n  }".into(),
            false,
        ),
        ("approved", "approved".into(), false),
        (
            "approvedBy",
            format!("approvedBy(first: {APPROVED_BY_CAP}) {{ nodes {{ username }} }}"),
            true,
        ),
        // Kept even when `minimal`: the ASSIGNED column is defined by this connection, and
        // dropping it would not blank the column, it would make it read "No" for merge
        // requests that are in fact assigned to you.
        (
            "assignees",
            format!("assignees(first: {ASSIGNEES_CAP}) {{ nodes {{ username }} }}"),
            false,
        ),
        (
            "reviewers",
            format!("reviewers(first: {REVIEWERS_CAP}) {{ nodes {{ username }} }}"),
            true,
        ),
        (
            "labels",
            format!("labels(first: {LABELS_CAP}) {{ nodes {{ title color }} }}"),
            true,
        ),
    ];

    let body: Vec<String> = selections
        .into_iter()
        .filter(|(name, _, connection)| {
            !(fragment.excludes(name) || (fragment.is_minimal() && *connection))
        })
        .map(|(_, rendered, _)| format!("  {rendered}"))
        .collect();

    format!(
        "fragment MrFields on MergeRequest {{\n{}\n}}\n",
        body.join("\n")
    )
}

/// GitLab's `MergeRequestState` enum value for a configured state filter.
const fn state_enum(state: StateFilter) -> &'static str {
    match state {
        StateFilter::Opened => "opened",
        StateFilter::Merged => "merged",
        StateFilter::Closed => "closed",
        StateFilter::All => "all",
    }
}

/// Accumulates variable declarations, call arguments and values together, so a variable
/// can never be declared without being used or passed without being declared.
#[derive(Default)]
struct Args {
    decls: Vec<String>,
    args: Vec<String>,
    values: Map<String, Value>,
}

impl Args {
    fn add(&mut self, name: &str, gql_type: &str, arg: &str, value: Value) {
        self.declare(name, gql_type, value);
        self.args.push(format!("{arg}: ${name}"));
    }

    /// A variable used somewhere other than the connection — `fullPath` belongs to the
    /// `group`/`project` root selection, and neither connection accepts it.
    fn declare(&mut self, name: &str, gql_type: &str, value: Value) {
        self.decls.push(format!("${name}: {gql_type}"));
        self.values.insert(name.to_owned(), value);
    }

    /// A variable that is declared and passed only when the filter sets it.
    fn add_opt(&mut self, name: &str, gql_type: &str, arg: &str, value: Option<Value>) {
        if let Some(value) = value {
            self.add(name, gql_type, arg, value);
        }
    }

    fn decls(&self) -> String {
        self.decls.join(", ")
    }

    fn args(&self) -> String {
        self.args.join(", ")
    }
}

/// Build the document for one filter page.
///
/// `now` is injected rather than read from the clock so the analyzer test produces the
/// same documents on every run.
pub fn build(
    filter: &Filter,
    fragment: &Fragment,
    page_size: u32,
    after: Option<&str>,
    now: Timestamp,
) -> Document {
    let mut a = Args::default();
    a.add("first", "Int!", "first", json!(page_size));
    a.add_opt("after", "String", "after", after.map(|c| json!(c)));

    // An enum literal rather than a variable: it is ours, not the user's, and inlining
    // keeps the document identical across filters, which the analyzer relies on.
    let sort = "sort: UPDATED_DESC";

    let state = json!(state_enum(filter.state));
    a.add("state", "MergeRequestState", "state", state);

    let (open_root, close_root, connection) = match filter.scope {
        Scope::Assigned => (String::new(), String::new(), "assignedMergeRequests"),
        Scope::ReviewRequested => (String::new(), String::new(), "reviewRequestedMergeRequests"),
        Scope::Authored => (String::new(), String::new(), "authoredMergeRequests"),
        Scope::Instance => (String::new(), String::new(), "mergeRequests"),
        Scope::Group | Scope::Project => {
            let root = if filter.scope == Scope::Group {
                "group"
            } else {
                "project"
            };
            a.declare(
                "fullPath",
                "ID!",
                json!(filter.path.clone().unwrap_or_default()),
            );
            (
                format!("{root}(fullPath: $fullPath) {{"),
                "}".to_owned(),
                "mergeRequests",
            )
        }
    };

    if !filter.scope.is_current_user() {
        add_scoped_arguments(&mut a, filter, now);
    }

    let selection = format!(
        "{connection}({args}, {sort}) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{ ...MrFields }}
    }}",
        args = a.args()
    );

    // `open_root`/`close_root` are empty for `Instance`: `mergeRequests` sits directly on
    // `Query`, with no wrapping root selection the way `group`/`project` need one.
    let body = if filter.scope.is_current_user() {
        format!("  currentUser {{\n    {selection}\n  }}")
    } else {
        format!("  {open_root}\n    {selection}\n  {close_root}")
    };

    let query = format!(
        "query MrqFilter({decls}) {{\n{body}\n}}\n\n{fields}",
        decls = a.decls(),
        fields = mr_fields(fragment)
    );

    Document {
        query,
        variables: Value::Object(a.values),
        page_size,
        fragment: fragment.clone(),
        label: format!(
            "{}/{}/first={page_size}",
            filter.scope.key(),
            fragment.label()
        ),
    }
}

/// Arguments accepted only by the group and project roots.
///
/// `validate` has already rejected these on a `currentUser` scope, so reaching here with
/// them set is impossible by construction.
fn add_scoped_arguments(a: &mut Args, filter: &Filter, now: Timestamp) {
    if filter.scope == Scope::Group {
        a.add(
            "includeSubgroups",
            "Boolean",
            "includeSubgroups",
            json!(filter.includes_subgroups()),
        );
    }

    if !filter.labels.is_empty() {
        a.add("labels", "[String!]", "labels", json!(filter.labels));
    }
    a.add_opt(
        "authorUsername",
        "String",
        "authorUsername",
        filter.author.as_ref().map(|v| json!(v)),
    );
    a.add_opt(
        "assigneeUsername",
        "String",
        "assigneeUsername",
        filter.assignee.as_ref().map(|v| json!(v)),
    );
    a.add_opt(
        "reviewerUsername",
        "String",
        "reviewerUsername",
        filter.reviewer.as_ref().map(|v| json!(v)),
    );
    a.add_opt(
        "reviewerWildcardId",
        "ReviewerWildcardId",
        "reviewerWildcardId",
        filter
            .has_reviewer
            .map(|any| json!(if any { "ANY" } else { "NONE" })),
    );
    a.add_opt(
        "milestoneTitle",
        "String",
        "milestoneTitle",
        filter.milestone.as_ref().map(|v| json!(v)),
    );
    // `targetBranches` is a list even for one branch; the config exposes the singular
    // because filtering on several target branches at once has no obvious use.
    a.add_opt(
        "targetBranches",
        "[String!]",
        "targetBranches",
        filter.target_branch.as_ref().map(|b| json!([b])),
    );

    if let Some(days) = filter.updated_after_days {
        // Hours, not days: a `Timestamp` is absolute and jiff refuses calendar units on
        // one, since "a day" is only well defined against a time zone.
        let since = now - Span::new().hours(i64::from(days) * 24);
        a.add(
            "updatedAfter",
            "Time",
            "updatedAfter",
            json!(since.to_string()),
        );
    }

    // Negated parameters go in one `not:` object rather than as separate arguments.
    if !filter.not_labels.is_empty() {
        a.add(
            "not",
            "MergeRequestsResolverNegatedParams",
            "not",
            json!({ "labels": filter.not_labels }),
        );
    }
}

/// Every document the binary can emit, for the coverage tests below.
///
/// Covers each scope, both fragments and every page size in the fallback ladder. A
/// maximally-argued group filter is included because arguments add to the cost.
#[cfg(test)]
pub(crate) fn all_documents(now: Timestamp) -> Vec<Document> {
    let mut filters = Vec::new();

    for scope in [Scope::Assigned, Scope::ReviewRequested, Scope::Authored] {
        filters.push(Filter::named("probe", scope));
    }

    for scope in [Scope::Group, Scope::Project] {
        let mut minimal = Filter::named("probe", scope);
        minimal.path = Some("group/project".into());
        filters.push(minimal.clone());

        // Worst case: every optional argument set.
        let maximal = Filter {
            labels: vec!["a".into(), "b".into()],
            not_labels: vec!["wip".into()],
            author: Some("u".into()),
            assignee: Some("u".into()),
            reviewer: Some("u".into()),
            has_reviewer: Some(false),
            milestone: Some("24.Q3".into()),
            target_branch: Some("main".into()),
            updated_after_days: Some(30),
            include_subgroups: Some(true),
            ..minimal
        };
        filters.push(maximal);
    }

    let instance_minimal = Filter::named("probe", Scope::Instance);
    filters.push(instance_minimal.clone());
    filters.push(Filter {
        labels: vec!["a".into(), "b".into()],
        not_labels: vec!["wip".into()],
        author: Some("u".into()),
        assignee: Some("u".into()),
        reviewer: Some("u".into()),
        has_reviewer: Some(false),
        milestone: Some("24.Q3".into()),
        target_branch: Some("main".into()),
        updated_after_days: Some(30),
        ..instance_minimal
    });

    let fragments = [Fragment::full(), Fragment::minimal()];

    let mut documents = Vec::new();
    for filter in &filters {
        for fragment in &fragments {
            for page_size in FALLBACK_PAGE_SIZES {
                documents.push(build(filter, fragment, page_size, None, now));
                // With a cursor too: `after` adds a variable and must also fit.
                documents.push(build(filter, fragment, page_size, Some("CURSOR"), now));
            }
        }
    }
    documents
}

/// The startup identity probe.
pub const CURRENT_USER_QUERY: &str = "\
query MrqCurrentUser {
  currentUser { id username name }
  metadata { version }
}
";

/// Number of times the connection is aliased in [`over_budget_probe`].
const PROBE_ALIASES: u32 = 20;

/// A document guaranteed to exceed any instance's complexity budget.
///
/// Aliasing the same connection several times multiplies the analyzed cost by the alias
/// count — the reason `build` never does it (§4.5's rule 1) is exactly why this can lean
/// on it deliberately: `mrq check` sends this once and reads the ceiling out of the
/// rejection (§4.5, "instance limit discovery").
pub fn over_budget_probe() -> Document {
    let aliases: String = (0..PROBE_ALIASES)
        .map(|i| format!("    p{i}: assignedMergeRequests(first: 100, state: opened) {{ nodes {{ ...MrFields }} }}\n"))
        .collect();

    let query = format!(
        "query MrqComplexityProbe {{\n  currentUser {{\n{aliases}  }}\n}}\n\n{fields}",
        fields = mr_fields(&Fragment::full())
    );

    Document {
        query,
        variables: Value::Object(Map::new()),
        page_size: 100,
        fragment: Fragment::full(),
        label: "complexity-probe".to_owned(),
    }
}

/// The GraphQL root a filter queries, formatted for a human rather than for the wire —
/// `mrq check`'s filter report, so a user can see what would be sent without reading a
/// GraphQL document.
pub fn root_description(filter: &Filter) -> String {
    let path = || filter.path.as_deref().unwrap_or("");
    match filter.scope {
        Scope::Assigned => "currentUser.assignedMergeRequests".to_owned(),
        Scope::ReviewRequested => "currentUser.reviewRequestedMergeRequests".to_owned(),
        Scope::Authored => "currentUser.authoredMergeRequests".to_owned(),
        Scope::Instance => "mergeRequests".to_owned(),
        Scope::Group => format!("group({:?}).mergeRequests", path()),
        Scope::Project => format!("project({:?}).mergeRequests", path()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-09-11T12:00:00Z".parse().unwrap()
    }

    fn doc(filter: &Filter) -> Document {
        build(filter, &Fragment::full(), DEFAULT_PAGE_SIZE, None, now())
    }

    fn group_filter() -> Filter {
        let mut f = Filter::named("Platform", Scope::Group);
        f.path = Some("acme/platform".into());
        f
    }

    /// Each scope maps onto a specific root and connection.
    #[test]
    fn each_scope_targets_its_documented_root() {
        let cases = [
            (Scope::Assigned, "currentUser", "assignedMergeRequests"),
            (
                Scope::ReviewRequested,
                "currentUser",
                "reviewRequestedMergeRequests",
            ),
            (Scope::Authored, "currentUser", "authoredMergeRequests"),
        ];
        for (scope, root, connection) in cases {
            let q = doc(&Filter::named("f", scope)).query;
            assert!(q.contains(root), "{scope:?}: {q}");
            assert!(q.contains(connection), "{scope:?}: {q}");
        }

        let q = doc(&group_filter()).query;
        assert!(q.contains("group(fullPath: $fullPath)"), "{q}");
        assert!(q.contains("mergeRequests("), "{q}");

        let mut project = group_filter();
        project.scope = Scope::Project;
        let q = doc(&project).query;
        assert!(q.contains("project(fullPath: $fullPath)"), "{q}");

        let q = doc(&Filter::named("f", Scope::Instance)).query;
        assert!(q.contains("mergeRequests("), "{q}");
        assert!(
            !q.contains("group(") && !q.contains("project(") && !q.contains("currentUser"),
            "instance scope selects the bare root: {q}"
        );
    }

    /// Every nested connection carries an explicit cap, or it is
    /// scored against the schema's much larger default page size.
    #[test]
    fn every_nested_connection_is_capped() {
        let q = doc(&Filter::named("f", Scope::Assigned)).query;

        for (field, cap) in [
            ("approvedBy", APPROVED_BY_CAP),
            ("assignees", ASSIGNEES_CAP),
            ("reviewers", REVIEWERS_CAP),
            ("labels", LABELS_CAP),
        ] {
            assert!(
                q.contains(&format!("{field}(first: {cap})")),
                "`{field}` must carry an explicit first: {q}"
            );
        }
    }

    /// A connection selected without `first:` would silently blow the budget, so assert
    /// the shape rather than trusting review.
    #[test]
    fn no_connection_is_selected_without_a_cap() {
        for fragment in [Fragment::full(), Fragment::minimal()] {
            let q = mr_fields(&fragment);
            for field in ["approvedBy", "assignees", "reviewers", "labels"] {
                if let Some(idx) = q.find(field) {
                    let after = &q[idx + field.len()..];
                    assert!(
                        after.starts_with("(first:"),
                        "`{field}` uncapped in the {} fragment",
                        fragment.label()
                    );
                }
            }
        }
    }

    #[test]
    fn the_full_fragment_selects_the_spec_field_set() {
        let q = doc(&Filter::named("f", Scope::Assigned)).query;
        for field in [
            "id",
            "iid",
            "title",
            "webUrl",
            "draft",
            "state",
            "createdAt",
            "updatedAt",
            "sourceBranch",
            "targetBranch",
            "conflicts",
            "mergeStatusEnum",
            "diffStatsSummary",
            "approved",
            "resolvableDiscussionsCount",
            "resolvedDiscussionsCount",
            "userNotesCount",
            "headPipeline",
            "detailedStatus",
        ] {
            assert!(q.contains(field), "missing `{field}`");
        }
    }

    /// `Project.webUrl` is never read — `WireProject` only deserializes `fullPath` — so
    /// selecting it just pays complexity budget for nothing. mrq-f7a.
    #[test]
    fn the_project_selection_does_not_ask_for_web_url() {
        let q = mr_fields(&Fragment::full());
        let start = q.find("project {").expect("project selection");
        let end = q[start..].find('}').unwrap() + start;
        assert!(!q[start..=end].contains("webUrl"), "{}", &q[start..=end]);
    }

    /// `minimal` exists for the complexity ladder, so it drops the nested connections
    /// and nothing else.
    #[test]
    fn the_minimal_fragment_drops_nested_connections() {
        let minimal = mr_fields(&Fragment::minimal());

        for dropped in ["approvedBy", "reviewers", "labels"] {
            assert!(!minimal.contains(dropped), "`{dropped}` survived reduction");
        }
        for kept in [
            "id",
            "title",
            "webUrl",
            "updatedAt",
            "diffStatsSummary",
            "headPipeline",
        ] {
            assert!(minimal.contains(kept), "`{kept}` should survive");
        }
        assert!(minimal.len() < mr_fields(&Fragment::full()).len());
    }

    /// The retry drops exactly what the server named and keeps the rest. Dropping a whole
    /// category instead would lose fields the instance actually supports.
    #[test]
    fn excluding_fields_removes_only_those_fields() {
        let fragment = Fragment::full().excluding(["userNotesCount", "resolvableDiscussionsCount"]);
        let rendered = mr_fields(&fragment);

        assert!(!rendered.contains("userNotesCount"), "{rendered}");
        assert!(
            !rendered.contains("resolvableDiscussionsCount"),
            "{rendered}"
        );
        for kept in ["approved", "approvedBy", "labels", "reviewers", "assignees"] {
            assert!(rendered.contains(kept), "`{kept}` should have survived");
        }
        assert!(fragment.is_degraded());
        assert!(!fragment.is_minimal());
    }

    #[test]
    fn exclusions_accumulate_and_a_full_fragment_is_not_degraded() {
        let base = Fragment::full();
        assert!(!base.is_degraded());

        let once = base.excluding(["userNotesCount"]);
        let twice = once.excluding(["labels"]);

        assert!(once.excludes("userNotesCount"));
        assert!(
            twice.excludes("userNotesCount"),
            "earlier exclusions are kept"
        );
        assert!(twice.excludes("labels"));
        assert!(
            !base.excludes("userNotesCount"),
            "excluding does not mutate the original"
        );
    }

    /// The status bar has to explain a degraded tab in the user's terms, not GraphQL's.
    #[test]
    fn a_degraded_fragment_names_what_the_user_lost() {
        assert_eq!(Fragment::full().lost(), None);

        let minimal = Fragment::minimal().lost().unwrap();
        assert!(minimal.contains("reviewers"), "{minimal}");
        assert!(minimal.contains("labels"), "{minimal}");
        assert!(
            !minimal.contains("assignees"),
            "assignees survives the reduction: {minimal}"
        );

        let excluded = Fragment::full()
            .excluding(["userNotesCount"])
            .lost()
            .unwrap();
        assert_eq!(excluded, "userNotesCount");
    }

    /// Dropping assignees would not blank the ASSIGNED column, it would make it read
    /// "No" for merge requests that are in fact assigned to you.
    #[test]
    fn the_reduced_fragment_keeps_assignees() {
        assert!(mr_fields(&Fragment::minimal()).contains("assignees(first:"));
    }

    /// Server-side sort, so a truncated set is the most recently touched.
    #[test]
    fn results_are_sorted_server_side_by_update_time() {
        assert!(
            doc(&Filter::named("f", Scope::Assigned))
                .query
                .contains("sort: UPDATED_DESC")
        );
    }

    #[test]
    fn paging_passes_the_cursor_as_a_variable() {
        let first = doc(&Filter::named("f", Scope::Assigned));
        assert!(
            !first.query.contains("$after"),
            "no cursor on the first page"
        );
        assert!(first.variables.get("after").is_none());

        let next = build(
            &Filter::named("f", Scope::Assigned),
            &Fragment::full(),
            DEFAULT_PAGE_SIZE,
            Some("CURSOR123"),
            now(),
        );
        assert!(next.query.contains("$after: String"), "{}", next.query);
        assert!(next.query.contains("after: $after"));
        assert_eq!(next.variables["after"], "CURSOR123");
    }

    /// The whole point of variables: a label containing a quote must not be able to
    /// produce a malformed document, which returns no data and shows an empty tab.
    #[test]
    fn filter_arguments_are_variables_not_interpolated() {
        let mut f = group_filter();
        f.labels = vec![r#"team::"platform" } evil {"#.into()];
        f.author = Some(r#"a" b"#.into());

        let d = doc(&f);
        assert!(
            !d.query.contains("evil"),
            "user input reached the document text: {}",
            d.query
        );
        assert_eq!(d.variables["labels"][0], r#"team::"platform" } evil {"#);
        assert_eq!(d.variables["authorUsername"], r#"a" b"#);
    }

    /// GraphQL rejects a document that declares a variable it never uses, so the two
    /// lists have to be built together.
    #[test]
    fn only_used_variables_are_declared() {
        let d = doc(&Filter::named("f", Scope::Assigned));

        let declared: Vec<&str> = ["labels", "authorUsername", "milestoneTitle", "fullPath"]
            .into_iter()
            .filter(|v| d.query.contains(&format!("${v}:")))
            .collect();
        assert!(declared.is_empty(), "unused declarations: {declared:?}");

        // And every declared variable has a value.
        for name in d.variables.as_object().unwrap().keys() {
            assert!(
                d.query.contains(&format!("${name}:")),
                "`{name}` passed but not declared"
            );
        }
    }

    #[test]
    fn every_declared_variable_is_also_referenced_as_an_argument() {
        let mut f = group_filter();
        f.labels = vec!["a".into()];
        f.not_labels = vec!["wip".into()];
        f.author = Some("u".into());
        f.milestone = Some("m".into());
        f.target_branch = Some("main".into());
        f.updated_after_days = Some(7);

        let d = doc(&f);
        for name in d.variables.as_object().unwrap().keys() {
            assert!(
                d.query.contains(&format!("${name}:")),
                "`{name}` has a value but no declaration: {}",
                d.query
            );
            // Referenced at least twice: once declared, once used as an argument.
            assert!(
                d.query.matches(&format!("${name}")).count() >= 2,
                "`{name}` is declared but never used: {}",
                d.query
            );
        }
    }

    #[test]
    fn scoped_arguments_map_to_gitlabs_names() {
        let mut f = group_filter();
        f.labels = vec!["team::platform".into()];
        f.not_labels = vec!["wip".into()];
        f.author = Some("jdoe".into());
        f.assignee = Some("asmith".into());
        f.reviewer = Some("bwayne".into());
        f.milestone = Some("24.Q3".into());
        f.target_branch = Some("main".into());

        let d = doc(&f);
        for arg in [
            "labels: $labels",
            "authorUsername: $authorUsername",
            "assigneeUsername: $assigneeUsername",
            "reviewerUsername: $reviewerUsername",
            "milestoneTitle: $milestoneTitle",
            "targetBranches: $targetBranches",
            "not: $not",
        ] {
            assert!(d.query.contains(arg), "missing `{arg}`: {}", d.query);
        }
        assert_eq!(
            d.variables["targetBranches"],
            json!(["main"]),
            "a list, even for one"
        );
        assert_eq!(d.variables["not"], json!({"labels": ["wip"]}));
    }

    #[test]
    fn has_reviewer_maps_to_the_wildcard_argument() {
        let mut f = group_filter();
        f.has_reviewer = Some(false);
        let d = doc(&f);
        assert!(d.query.contains("reviewerWildcardId: $reviewerWildcardId"));
        assert_eq!(d.variables["reviewerWildcardId"], json!("NONE"));

        f.has_reviewer = Some(true);
        let d = doc(&f);
        assert_eq!(d.variables["reviewerWildcardId"], json!("ANY"));

        f.has_reviewer = None;
        let d = doc(&f);
        assert!(!d.query.contains("reviewerWildcardId"));
        assert!(d.variables.get("reviewerWildcardId").is_none());
    }

    /// The instance scope takes the same root-only arguments as group/project, but no
    /// `fullPath` — `Query.mergeRequests` needs no root selection to declare it against.
    #[test]
    fn instance_scope_takes_root_only_arguments_without_a_path() {
        let mut f = Filter::named("f", Scope::Instance);
        f.labels = vec!["sre-review::ask".into()];
        f.has_reviewer = Some(false);

        let d = doc(&f);
        assert!(d.query.contains("labels: $labels"), "{}", d.query);
        assert!(
            d.query.contains("reviewerWildcardId: $reviewerWildcardId"),
            "{}",
            d.query
        );
        // `Project.fullPath` in the fragment is unrelated: this checks the root-selecting
        // `$fullPath` variable that `group`/`project` scopes declare, not that field.
        assert!(!d.query.contains("$fullPath"), "{}", d.query);
        assert!(d.variables.get("fullPath").is_none());
    }

    /// `fullPath` selects the root, and neither `Group.mergeRequests` nor
    /// `Project.mergeRequests` accepts it. Passing it to the connection is a validation
    /// failure, which returns no data at all rather than less data.
    #[test]
    fn full_path_is_passed_to_the_root_only() {
        for scope in [Scope::Group, Scope::Project] {
            let mut f = group_filter();
            f.scope = scope;
            let q = doc(&f).query;

            let root = if scope == Scope::Group {
                "group"
            } else {
                "project"
            };
            assert!(q.contains(&format!("{root}(fullPath: $fullPath)")), "{q}");
            assert_eq!(
                q.matches("fullPath: $fullPath").count(),
                1,
                "`fullPath` reached the connection arguments: {q}"
            );
        }
    }

    #[test]
    fn include_subgroups_applies_to_group_scope_only() {
        let d = doc(&group_filter());
        assert!(d.query.contains("includeSubgroups: $includeSubgroups"));
        assert_eq!(
            d.variables["includeSubgroups"],
            json!(true),
            "defaults to true"
        );

        let mut project = group_filter();
        project.scope = Scope::Project;
        let d = doc(&project);
        assert!(!d.query.contains("includeSubgroups"), "{}", d.query);
    }

    #[test]
    fn updated_after_becomes_an_absolute_timestamp() {
        let mut f = group_filter();
        f.updated_after_days = Some(30);

        let d = doc(&f);
        let since = d.variables["updatedAfter"].as_str().unwrap();
        assert!(
            since.starts_with("2026-08-12"),
            "30 days before now(): {since}"
        );
    }

    #[test]
    fn state_is_passed_for_every_scope() {
        for state in [
            StateFilter::Opened,
            StateFilter::Merged,
            StateFilter::Closed,
            StateFilter::All,
        ] {
            let mut f = Filter::named("f", Scope::Assigned);
            f.state = state;
            let d = doc(&f);
            assert_eq!(d.variables["state"], json!(state_enum(state)));
            assert!(d.query.contains("state: $state"));
        }
    }

    /// The analyzer checks every document the binary can emit.
    #[test]
    fn all_documents_covers_every_scope_fragment_and_page_size() {
        let docs = all_documents(now());
        assert!(!docs.is_empty());

        for scope in [
            Scope::Assigned,
            Scope::ReviewRequested,
            Scope::Authored,
            Scope::Group,
            Scope::Project,
        ] {
            assert!(
                docs.iter().any(|d| d.label.starts_with(scope.key())),
                "no document for {scope:?}"
            );
        }
        for fragment in [Fragment::full(), Fragment::minimal()] {
            assert!(docs.iter().any(|d| d.fragment == fragment));
        }
        for size in FALLBACK_PAGE_SIZES {
            assert!(docs.iter().any(|d| d.page_size == size));
        }
        assert!(
            docs.iter().any(|d| d.variables.get("after").is_some()),
            "a cursor adds a variable and must also be measured"
        );
    }

    #[test]
    fn documents_are_deterministic_for_a_fixed_clock() {
        assert_eq!(all_documents(now()), all_documents(now()));
    }

    #[test]
    fn the_page_size_ladder_descends_from_the_default() {
        assert_eq!(FALLBACK_PAGE_SIZES[0], DEFAULT_PAGE_SIZE);
        for pair in FALLBACK_PAGE_SIZES.windows(2) {
            assert!(pair[1] < pair[0], "the ladder must descend");
        }
        assert_eq!(
            *FALLBACK_PAGE_SIZES.last().unwrap(),
            5,
            "the documented page-size floor"
        );
    }

    #[test]
    fn the_identity_probe_asks_for_username_and_version() {
        assert!(CURRENT_USER_QUERY.contains("currentUser"));
        assert!(CURRENT_USER_QUERY.contains("username"));
        assert!(CURRENT_USER_QUERY.contains("metadata"));
        assert!(CURRENT_USER_QUERY.contains("version"));
    }

    /// The whole point of the probe is a document no instance would accept, so it must
    /// alias the connection more than once and ask for the full fragment.
    #[test]
    fn the_over_budget_probe_aliases_the_connection_many_times() {
        let probe = over_budget_probe();
        assert_eq!(
            probe.query.matches("assignedMergeRequests").count(),
            PROBE_ALIASES as usize
        );
        assert!(!probe.fragment.is_minimal());
        assert!(probe.query.contains("...MrFields"));
        assert_eq!(probe.variables, Value::Object(Map::new()));
    }

    #[test]
    fn root_description_names_the_actual_graphql_root() {
        assert_eq!(
            root_description(&Filter::named("f", Scope::Assigned)),
            "currentUser.assignedMergeRequests"
        );
        assert_eq!(
            root_description(&Filter::named("f", Scope::Instance)),
            "mergeRequests"
        );

        let mut group = Filter::named("f", Scope::Group);
        group.path = Some("acme/platform".into());
        assert_eq!(
            root_description(&group),
            "group(\"acme/platform\").mergeRequests"
        );
    }

    /// The deepest path must stay well under the limit of 15.
    #[test]
    fn nesting_depth_stays_shallow() {
        let d = doc(&group_filter());
        let mut depth = 0usize;
        let mut max = 0usize;
        for c in d.query.chars() {
            match c {
                '{' => {
                    depth += 1;
                    max = max.max(depth);
                }
                '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        assert!(max <= 8, "brace depth {max} is deeper than expected");
    }
}
