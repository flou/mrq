//! Semantic validation: the rules that type-checking cannot express.
//!
//! `schema` answers "does it parse"; this answers "does it mean anything". The split
//! matters because the two failure modes read differently — a parse error points at a
//! line, a semantic error has to explain a rule.
//!
//! # Reject or clamp
//!
//! The line is drawn by who gets hurt. A `max_results` of 5000 only affects
//! the person who typed it, so it clamps with a warning and `mrq` still starts.
//! A `refresh.interval_secs` of 1 hammers a shared GitLab instance on everyone else's
//! behalf, so it is a hard error even though the intent is obvious.
//!
//! Errors accumulate where they can. A config with three mistakes should report three,
//! not make the user fix one and re-run to discover the next.

use std::collections::BTreeSet;
use std::fmt;

use crate::config::schema::{Column, Config, Filter, Scope};
use crate::error::ConfigError;

/// Below this, refreshing is abusive to a shared instance.
pub const MIN_INTERVAL_SECS: u64 = 30;

const MIN_MAX_RESULTS: usize = 1;
const MAX_MAX_RESULTS: usize = 500;

/// A value that was out of range and has been clamped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clamped {
    pub key: String,
    pub from: String,
    pub to: String,
}

impl fmt::Display for Clamped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} is out of range, using {}",
            self.key, self.from, self.to
        )
    }
}

/// Validate and clamp in place, returning the clamps for the caller to warn about.
pub fn validate(config: &mut Config) -> Result<Vec<Clamped>, ConfigError> {
    let mut clamps = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    check_gitlab(config, &mut errors);
    check_refresh(config, &mut errors, &mut clamps);
    check_ui_and_sort(config, &mut errors);
    check_notifications(config, &mut errors);
    check_filters(config, &mut errors, &mut clamps);

    if let Some(first) = errors.first() {
        // One error per line, under one ConfigError, so a config with several mistakes
        // reports all of them in a single run.
        return Err(ConfigError::Invalid {
            key: "config".into(),
            message: if errors.len() == 1 {
                first.clone()
            } else {
                format!("\n  - {}", errors.join("\n  - "))
            },
        });
    }

    Ok(clamps)
}

fn check_gitlab(config: &Config, errors: &mut Vec<String>) {
    let url = config.gitlab.url.trim();
    if url.is_empty() {
        errors.push("gitlab.url: must not be empty".into());
    } else if !url.starts_with("http://") && !url.starts_with("https://") {
        // A bare host is the common mistake and produces an inscrutable transport error
        // several layers down, so it is worth catching with a concrete suggestion.
        errors.push(format!(
            "gitlab.url: `{url}` must start with http:// or https:// (try `https://{url}`)"
        ));
    }

    if config.gitlab.timeout_secs == 0 {
        errors.push("gitlab.timeout_secs: must be at least 1".into());
    }
    if config.gitlab.max_concurrent_requests == 0 {
        errors.push("gitlab.max_concurrent_requests: must be at least 1".into());
    }
}

fn check_refresh(config: &mut Config, errors: &mut Vec<String>, clamps: &mut Vec<Clamped>) {
    // A hard error, deliberately not a clamp. Silently correcting it would leave the
    // user believing they are refreshing every 5 seconds.
    let interval = config.refresh.interval_secs;
    if interval < MIN_INTERVAL_SECS {
        errors.push(format!(
            "refresh.interval_secs: {interval} is below the minimum of {MIN_INTERVAL_SECS} \
             seconds; polling faster than that is abusive to a shared GitLab instance"
        ));
    }

    // Jitter beyond the interval would make refresh timing unpredictable rather than
    // merely staggered, which is the opposite of what it is for.
    if config.refresh.jitter_secs > interval && interval > 0 {
        clamps.push(Clamped {
            key: "refresh.jitter_secs".into(),
            from: config.refresh.jitter_secs.to_string(),
            to: interval.to_string(),
        });
        config.refresh.jitter_secs = interval;
    }
}

fn check_ui_and_sort(config: &Config, errors: &mut Vec<String>) {
    if config.ui.columns.is_empty() {
        errors.push("ui.columns: must list at least one column".into());
    }

    let mut seen = BTreeSet::new();
    for column in &config.ui.columns {
        if !seen.insert(*column) {
            errors.push(format!(
                "ui.columns: `{}` is listed more than once",
                column.key()
            ));
        }
    }

    // The sort column has to be one the user can actually see: sorting by a column that
    // is not displayed leaves the status bar naming one the user cannot see, with no way
    // to tell why the order looks arbitrary.
    if !config.ui.columns.contains(&config.sort.column) {
        errors.push(format!(
            "sort.column: `{}` is not in ui.columns ({})",
            config.sort.column.key(),
            config
                .ui
                .columns
                .iter()
                .map(|c| c.key())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // The columns that identify a row are never allowed to hide behind wide mode.
    const NEVER_WIDE: [Column; 5] = [
        Column::Approved,
        Column::Author,
        Column::Repo,
        Column::Title,
        Column::Pipeline,
    ];

    let mut seen_wide = BTreeSet::new();
    for column in &config.ui.wide_columns {
        if !seen_wide.insert(*column) {
            errors.push(format!(
                "ui.wide_columns: `{}` is listed more than once",
                column.key()
            ));
        }
        if !config.ui.columns.contains(column) {
            errors.push(format!(
                "ui.wide_columns: `{}` is not in ui.columns",
                column.key()
            ));
        }
        if NEVER_WIDE.contains(column) {
            errors.push(format!(
                "ui.wide_columns: `{}` identifies a row and can never be wide-only",
                column.key()
            ));
        }
    }
}

fn check_notifications(config: &Config, errors: &mut Vec<String>) {
    use crate::config::schema::{NotifyBackend, StateFilter};

    if config.notifications.backend == NotifyBackend::Command
        && config.notifications.command.trim().is_empty()
    {
        errors.push(
            "notifications.command: required when notifications.backend = \"command\"".into(),
        );
    }

    // `state = "opened"` never sees a merge request transition to merged or closed —
    // it just leaves the result set — so the trigger would fire silently never. A
    // filter at "all", "merged" or "closed" can observe the transition.
    if config.notifications.on_merged_or_closed
        && config
            .filters
            .iter()
            .all(|f| f.state == StateFilter::Opened)
    {
        errors.push(
            "notifications.on_merged_or_closed: true, but every filter has state = \"opened\", \
             which never observes a merge or close; add a filter with state = \"all\", \
             \"merged\" or \"closed\""
                .into(),
        );
    }
}

fn check_filters(config: &mut Config, errors: &mut Vec<String>, clamps: &mut Vec<Clamped>) {
    if config.filters.is_empty() {
        errors.push("filter: at least one [[filter]] is required".into());
        return;
    }

    let mut names = BTreeSet::new();
    let mut slugs = BTreeSet::new();

    for (i, filter) in config.filters.iter_mut().enumerate() {
        let at = format!("filter[{i}]");

        if filter.name.trim().is_empty() {
            errors.push(format!("{at}.name: must not be empty"));
        } else if !names.insert(filter.name.clone()) {
            // Names address tabs and the cache file, so duplicates are ambiguous in two
            // different ways.
            errors.push(format!(
                "{at}.name: `{}` is used by more than one filter",
                filter.name
            ));
        } else if !slugs.insert(filter.slug()) {
            errors.push(format!(
                "{at}.name: `{}` collides with another filter's cache file",
                filter.name
            ));
        }

        check_filter_scope(filter, &at, errors);

        let clamped = filter.max_results.clamp(MIN_MAX_RESULTS, MAX_MAX_RESULTS);
        if clamped != filter.max_results {
            clamps.push(Clamped {
                key: format!("{at}.max_results"),
                from: filter.max_results.to_string(),
                to: clamped.to_string(),
            });
            filter.max_results = clamped;
        }
    }
}

fn check_filter_scope(filter: &Filter, at: &str, errors: &mut Vec<String>) {
    if filter.scope.requires_path() {
        match filter.path.as_deref().map(str::trim) {
            None | Some("") => errors.push(format!(
                "{at}.path: required for scope = \"{}\"",
                filter.scope.key()
            )),
            Some(path) if path.starts_with('/') || path.ends_with('/') => errors.push(format!(
                "{at}.path: `{path}` should be a GitLab full path like `group/project`, \
                 without leading or trailing slashes"
            )),
            Some(_) => {}
        }
    } else if filter.path.is_some() {
        errors.push(format!(
            "{at}.path: not accepted for scope = \"{}\"; it applies to the group and \
             project scopes only",
            filter.scope.key()
        ));
    }

    if filter.scope != Scope::Group && filter.include_subgroups.is_some() {
        errors.push(format!(
            "{at}.include_subgroups: applies to scope = \"group\" only"
        ));
    }

    // GitLab's `reviewerUsername` and `reviewerWildcardId` arguments are mutually
    // exclusive; accepting both here would silently drop one rather than erroring.
    if filter.reviewer.is_some() && filter.has_reviewer.is_some() {
        errors.push(format!(
            "{at}: `reviewer` and `has_reviewer` cannot both be set; keep one"
        ));
    }

    // The `currentUser.*` connections do not accept these arguments, so a filter that
    // sets them would silently return the unfiltered list. The message names the
    // alternative because "not supported" on its own leaves the user stuck.
    if filter.scope.is_current_user() {
        let offenders = filter.root_only_args();
        if !offenders.is_empty() {
            errors.push(format!(
                "{at}: {} cannot be combined with scope = \"{}\"; they are only accepted \
                 by the group, project and instance query roots. Use scope = \"group\" \
                 with a `path`, or scope = \"instance\" to search across every project the \
                 token can see.",
                offenders
                    .iter()
                    .map(|o| format!("`{o}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                filter.scope.key()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Column, NotifyBackend, Scope, StateFilter};

    fn config() -> Config {
        Config::default()
    }

    fn err_of(mut c: Config) -> String {
        validate(&mut c).unwrap_err().to_string()
    }

    #[test]
    fn the_built_in_defaults_are_valid() {
        let mut c = config();
        let clamps = validate(&mut c).unwrap();
        assert!(
            clamps.is_empty(),
            "defaults should need no clamping: {clamps:?}"
        );
    }

    /// A hard error, deliberately not a clamp. Correcting it silently would
    /// leave the user believing they refresh every 5 seconds.
    #[test]
    fn a_sub_minimum_interval_is_rejected_not_clamped() {
        let mut c = config();
        c.refresh.interval_secs = 5;

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("interval_secs"), "{err}");
        assert!(err.contains("30"), "should name the minimum: {err}");
        assert_eq!(
            c.refresh.interval_secs, 5,
            "value must not be silently fixed"
        );
    }

    #[test]
    fn the_minimum_interval_itself_is_accepted() {
        let mut c = config();
        c.refresh.interval_secs = MIN_INTERVAL_SECS;
        assert!(validate(&mut c).is_ok());
    }

    /// Out-of-range values that only affect the person who typed them clamp and warn.
    #[test]
    fn max_results_clamps_per_filter() {
        let mut c = config();
        c.filters[0].max_results = 5_000;

        let clamps = validate(&mut c).unwrap();
        assert_eq!(c.filters[0].max_results, MAX_MAX_RESULTS);
        assert_eq!(clamps[0].key, "filter[0].max_results");

        let mut c = config();
        c.filters[0].max_results = 0;
        validate(&mut c).unwrap();
        assert_eq!(c.filters[0].max_results, MIN_MAX_RESULTS);
    }

    /// Jitter larger than the interval makes refreshes unpredictable rather than merely
    /// staggered, which is the opposite of its purpose.
    #[test]
    fn jitter_larger_than_the_interval_clamps() {
        let mut c = config();
        c.refresh.interval_secs = 60;
        c.refresh.jitter_secs = 600;

        let clamps = validate(&mut c).unwrap();
        assert_eq!(c.refresh.jitter_secs, 60);
        assert_eq!(clamps[0].key, "refresh.jitter_secs");
    }

    /// The single most likely configuration mistake: GitLab's GraphQL API has no
    /// instance-wide MR search, so a label filter needs a group scope.
    #[test]
    fn root_only_arguments_on_a_current_user_scope_are_rejected_with_the_alternative() {
        for scope in [Scope::Assigned, Scope::ReviewRequested, Scope::Authored] {
            let mut c = config();
            c.filters[0].scope = scope;
            c.filters[0].labels = vec!["team::platform".into()];

            let err = validate(&mut c).unwrap_err().to_string();
            assert!(err.contains("labels"), "{err}");
            assert!(err.contains(scope.key()), "{err}");
            assert!(
                err.contains("group"),
                "must name the group-scope alternative: {err}"
            );
        }
    }

    #[test]
    fn every_offending_argument_is_named_at_once() {
        let mut c = config();
        c.filters[0].labels = vec!["a".into()];
        c.filters[0].milestone = Some("24.Q3".into());
        c.filters[0].target_branch = Some("main".into());

        let err = validate(&mut c).unwrap_err().to_string();
        for key in ["labels", "milestone", "target_branch"] {
            assert!(err.contains(key), "missing `{key}` in: {err}");
        }
    }

    #[test]
    fn the_same_arguments_are_fine_on_a_group_scope() {
        let mut c = config();
        c.filters[0].scope = Scope::Group;
        c.filters[0].path = Some("acme/platform".into());
        c.filters[0].labels = vec!["team::platform".into()];
        c.filters[0].milestone = Some("24.Q3".into());

        assert!(validate(&mut c).is_ok());
    }

    #[test]
    fn has_reviewer_on_a_current_user_scope_is_rejected() {
        let mut c = config();
        c.filters[0].has_reviewer = Some(false);

        let err = err_of(c);
        assert!(err.contains("has_reviewer"), "{err}");
    }

    #[test]
    fn reviewer_and_has_reviewer_together_are_rejected() {
        let mut c = config();
        c.filters[0].scope = Scope::Group;
        c.filters[0].path = Some("acme/platform".into());
        c.filters[0].reviewer = Some("someuser".into());
        c.filters[0].has_reviewer = Some(false);

        let err = err_of(c);
        assert!(err.contains("reviewer"), "{err}");
        assert!(err.contains("has_reviewer"), "{err}");
    }

    #[test]
    fn has_reviewer_alone_is_fine_on_a_group_scope() {
        let mut c = config();
        c.filters[0].scope = Scope::Group;
        c.filters[0].path = Some("acme/platform".into());
        c.filters[0].has_reviewer = Some(false);

        assert!(validate(&mut c).is_ok());
    }

    #[test]
    fn labels_and_has_reviewer_are_fine_on_the_instance_scope_without_a_path() {
        let mut c = config();
        c.filters[0].scope = Scope::Instance;
        c.filters[0].labels = vec!["sre-review::ask".into()];
        c.filters[0].has_reviewer = Some(false);

        assert!(validate(&mut c).is_ok());
    }

    #[test]
    fn a_path_on_the_instance_scope_is_rejected() {
        let mut c = config();
        c.filters[0].scope = Scope::Instance;
        c.filters[0].path = Some("acme/platform".into());

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("path"), "{err}");
    }

    #[test]
    fn group_and_project_scopes_require_a_path() {
        for scope in [Scope::Group, Scope::Project] {
            let mut c = config();
            c.filters[0].scope = scope;
            c.filters[0].path = None;

            let err = validate(&mut c).unwrap_err().to_string();
            assert!(err.contains("path"), "{err}");
            assert!(err.contains(scope.key()), "{err}");
        }
    }

    #[test]
    fn a_path_on_a_current_user_scope_is_rejected() {
        let mut c = config();
        c.filters[0].scope = Scope::Assigned;
        c.filters[0].path = Some("acme/platform".into());

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("path"), "{err}");
    }

    #[test]
    fn a_malformed_path_is_rejected() {
        let mut c = config();
        c.filters[0].scope = Scope::Group;
        c.filters[0].path = Some("/acme/platform/".into());

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("slashes"), "{err}");
    }

    /// Names address both a tab and a cache file, so a duplicate is ambiguous twice over.
    #[test]
    fn duplicate_filter_names_are_rejected() {
        let mut c = config();
        c.filters = vec![
            Filter::named("Mine", Scope::Assigned),
            Filter::named("Mine", Scope::Authored),
        ];

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("Mine"), "{err}");
        assert!(err.contains("more than one"), "{err}");
    }

    #[test]
    fn an_empty_filter_name_is_rejected() {
        let mut c = config();
        c.filters = vec![Filter::named("  ", Scope::Assigned)];
        assert!(err_of(c).contains("name"));
    }

    #[test]
    fn sort_column_must_be_displayed() {
        let mut c = config();
        c.ui.columns = vec![Column::Title, Column::Author];
        c.sort.column = Column::Diff;

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("sort.column"), "{err}");
        assert!(err.contains("diff"), "{err}");
        assert!(
            err.contains("title"),
            "should list what is available: {err}"
        );
    }

    #[test]
    fn duplicate_and_empty_column_lists_are_rejected() {
        let mut c = config();
        c.ui.columns = vec![Column::Title, Column::Title];
        assert!(err_of(c).contains("more than once"));

        let mut c = config();
        c.ui.columns = vec![];
        assert!(err_of(c).contains("at least one"));
    }

    #[test]
    fn wide_columns_must_be_displayed_columns() {
        let mut c = config();
        c.ui.columns = vec![Column::Title, Column::Author];
        c.ui.wide_columns = vec![Column::Diff];

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("wide_columns"), "{err}");
        assert!(err.contains("diff"), "{err}");
    }

    #[test]
    fn duplicate_wide_columns_are_rejected() {
        let mut c = config();
        c.ui.wide_columns = vec![Column::Diff, Column::Diff];
        assert!(err_of(c).contains("more than once"));
    }

    #[test]
    fn a_row_identifying_column_can_never_be_wide_only() {
        for column in [
            Column::Approved,
            Column::Author,
            Column::Repo,
            Column::Title,
            Column::Pipeline,
        ] {
            let mut c = config();
            c.ui.wide_columns = vec![column];

            let err = validate(&mut c).unwrap_err().to_string();
            assert!(err.contains("wide_columns"), "{column:?}: {err}");
            assert!(err.contains(column.key()), "{column:?}: {err}");
        }
    }

    #[test]
    fn a_bare_host_url_is_rejected_with_a_suggestion() {
        let mut c = config();
        c.gitlab.url = "gitlab.example.com".into();

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("https://gitlab.example.com"), "{err}");
    }

    #[test]
    fn zero_timeout_and_concurrency_are_rejected() {
        let mut c = config();
        c.gitlab.timeout_secs = 0;
        assert!(err_of(c).contains("timeout_secs"));

        let mut c = config();
        c.gitlab.max_concurrent_requests = 0;
        assert!(err_of(c).contains("max_concurrent_requests"));
    }

    #[test]
    fn command_backend_requires_a_command() {
        let mut c = config();
        c.notifications.backend = NotifyBackend::Command;
        c.notifications.command = String::new();
        assert!(err_of(c).contains("notifications.command"));

        let mut c = config();
        c.notifications.backend = NotifyBackend::Command;
        c.notifications.command = "notify-send {title} {body}".into();
        assert!(validate(&mut c).is_ok());
    }

    /// `state = "opened"` never sees a merge request transition — it just drops out of
    /// the result set — so this trigger would fire silently never. mrq-rve.
    #[test]
    fn on_merged_or_closed_is_rejected_when_every_filter_stays_opened() {
        let mut c = config();
        c.notifications.on_merged_or_closed = true;
        c.filters[0].state = StateFilter::Opened;

        let err = err_of(c);
        assert!(err.contains("on_merged_or_closed"), "{err}");
        assert!(err.contains("opened"), "{err}");
    }

    #[test]
    fn on_merged_or_closed_is_fine_once_one_filter_can_observe_the_transition() {
        let mut c = config();
        c.notifications.on_merged_or_closed = true;
        c.filters[0].state = StateFilter::Opened;
        c.filters.push(Filter {
            name: "All".into(),
            state: StateFilter::All,
            ..Filter::default()
        });

        assert!(validate(&mut c).is_ok());
    }

    /// A config with three mistakes should report three, not make the user fix one and
    /// re-run to discover the next.
    #[test]
    fn multiple_errors_are_reported_together() {
        let mut c = config();
        c.refresh.interval_secs = 1;
        c.gitlab.url = "nope".into();
        c.filters[0].labels = vec!["x".into()];

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(err.contains("interval_secs"), "{err}");
        assert!(err.contains("gitlab.url"), "{err}");
        assert!(err.contains("labels"), "{err}");
        assert_eq!(err.matches("\n  - ").count(), 3, "one line each: {err}");
    }

    #[test]
    fn a_single_error_is_reported_without_list_formatting() {
        let mut c = config();
        c.refresh.interval_secs = 1;

        let err = validate(&mut c).unwrap_err().to_string();
        assert!(!err.contains("\n  - "), "{err}");
    }
}
