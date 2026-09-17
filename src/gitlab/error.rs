//! Classifying GraphQL failures, and the per-filter retry policy.
//!
//! The transport in [`super::client`] classifies what HTTP can tell it. This module
//! classifies what only the response body can: a GraphQL request that failed returns
//! HTTP 200 with an `errors` array, so the status line says nothing.
//!
//! # Order matters
//!
//! GraphQL validates a document before analysing its cost. A query that is both
//! over-budget *and* references a field the instance does not have reports only the
//! unknown field, so the field fallback has to run first — otherwise the complexity
//! ladder would appear to work while never firing. Measured against a real instance,
//! which reported two undefined fields and said nothing about complexity.
//!
//! # Codes, not prose
//!
//! Classification reads `extensions.code` where GitLab supplies one. Error messages are
//! prose, get reworded between releases, and are localised on some instances; a
//! classifier built on substring matching silently stops recognising a failure class
//! after an upgrade, and the symptom is an empty tab rather than an error.

use std::time::Duration;

use crate::error::{Error, GraphQlError, LimitKind};

/// `extensions.code` for a field that does not exist on the type.
const CODE_UNDEFINED_FIELD: &str = "undefinedField";

/// Classify the `errors` array of an HTTP 200 GraphQL response.
///
/// `has_data` distinguishes a partial response — some of the query succeeded, and the
/// spec requires rendering what arrived — from a total failure.
pub fn classify(errors: &[GraphQlError], has_data: bool) -> Option<Error> {
    if errors.is_empty() {
        return None;
    }

    // Validation errors first: they mask everything else, because the server never got
    // as far as analysing cost.
    let fields = unknown_fields(errors);
    if !fields.is_empty() {
        return Some(Error::UnknownField { fields });
    }

    if let Some((kind, limit)) = limit_exceeded(errors) {
        return Some(Error::LimitExceeded { kind, limit });
    }

    if has_data {
        return Some(Error::GraphQlPartial {
            errors: errors.to_vec(),
        });
    }
    Some(Error::GraphQl {
        errors: errors.to_vec(),
    })
}

/// Every field the server rejected as undefined, deduplicated and in a stable order.
///
/// Prefers `extensions.fieldName`; falls back to the quoted name in the message for
/// instances that do not populate extensions.
pub fn unknown_fields(errors: &[GraphQlError]) -> Vec<String> {
    let mut fields = Vec::new();
    for error in errors {
        let is_undefined = error.code.as_deref() == Some(CODE_UNDEFINED_FIELD)
            || (error.code.is_none() && error.message.contains("doesn't exist on type"));
        if !is_undefined {
            continue;
        }

        let name = error
            .field
            .clone()
            .or_else(|| quoted_name(&error.message))
            .unwrap_or_default();

        if !name.is_empty() && !fields.contains(&name) {
            fields.push(name);
        }
    }
    fields
}

/// The first single-quoted identifier in a message, e.g. `Field 'approvalsLeft' ...`.
fn quoted_name(message: &str) -> Option<String> {
    let rest = message.split_once('\'')?.1;
    let (name, _) = rest.split_once('\'')?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Whether the server rejected the document for cost, and the limit it reported.
fn limit_exceeded(errors: &[GraphQlError]) -> Option<(LimitKind, Option<u32>)> {
    for error in errors {
        let lower = error.message.to_ascii_lowercase();
        let kind = if lower.contains("complexity") {
            LimitKind::Complexity
        } else if lower.contains("depth") {
            LimitKind::Depth
        } else {
            continue;
        };
        if !lower.contains("exceed") && !lower.contains("max") {
            continue;
        }
        // "which exceeds max complexity of 250" — the last number is the limit; the
        // first is the query's own score, which is not what a caller needs.
        return Some((kind, last_number(&error.message)));
    }
    None
}

fn last_number(text: &str) -> Option<u32> {
    text.split(|c: char| !c.is_ascii_digit())
        .rfind(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

/// Exponential backoff for one filter's fetch loop.
///
/// Capped at the refresh interval: backing off longer than the interval would mean a
/// filter that failed once refreshes *less* often than a healthy one indefinitely, and
/// the user has no way to tell that from the feature simply not working.
#[derive(Debug, Clone)]
pub struct Backoff {
    attempt: u32,
    base: Duration,
    cap: Duration,
}

/// First retry delay: 2s → 4s → ….
const BASE_DELAY: Duration = Duration::from_secs(2);

impl Backoff {
    pub fn new(cap: Duration) -> Self {
        Self {
            attempt: 0,
            base: BASE_DELAY,
            cap: cap.max(BASE_DELAY),
        }
    }

    /// Consecutive failures so far.
    #[cfg(test)]
    const fn attempt(&self) -> u32 {
        self.attempt
    }

    #[cfg(test)]
    pub(crate) const fn is_failing(&self) -> bool {
        self.attempt > 0
    }

    /// Clear the failure streak after a successful fetch.
    pub const fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Record a failure and return how long to wait.
    ///
    /// A server-supplied `retry_after` wins outright: it knows when the rate limit
    /// window closes and we do not, and ignoring it is how a client gets banned.
    pub fn record_failure(&mut self, retry_after: Option<Duration>) -> Duration {
        self.attempt = self.attempt.saturating_add(1);

        if let Some(server) = retry_after {
            return server;
        }

        // 2, 4, 8, … capped. Shift rather than powi to avoid overflowing on a filter
        // that has been failing for days.
        let factor = 1u64.checked_shl(self.attempt - 1).unwrap_or(u64::MAX);
        let delay = self
            .base
            .checked_mul(factor.min(u32::MAX as u64) as u32)
            .unwrap_or(self.cap);
        delay.min(self.cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn undefined(field: &str) -> GraphQlError {
        GraphQlError {
            message: format!("Field '{field}' doesn't exist on type 'MergeRequest'"),
            path: Some(format!("fragment MrFields.{field}")),
            code: Some(CODE_UNDEFINED_FIELD.to_owned()),
            field: Some(field.to_owned()),
        }
    }

    #[test]
    fn no_errors_is_no_failure() {
        assert!(classify(&[], true).is_none());
        assert!(classify(&[], false).is_none());
    }

    /// The exact pair a real instance returned.
    #[test]
    fn undefined_fields_are_collected_from_extensions() {
        let errors = [undefined("approvalsRequired"), undefined("approvalsLeft")];

        match classify(&errors, false) {
            Some(Error::UnknownField { fields }) => {
                assert_eq!(fields, ["approvalsRequired", "approvalsLeft"]);
            }
            other => panic!("expected UnknownField, got {other:?}"),
        }
    }

    /// Both offenders must come back from one response; retrying per field would cost a
    /// round trip each and, on a slow instance, several seconds of blank table.
    #[test]
    fn one_response_reports_every_offending_field() {
        let errors = [
            undefined("approvalsRequired"),
            undefined("approvalsLeft"),
            undefined("approvalsRequired"),
        ];
        assert_eq!(unknown_fields(&errors).len(), 2, "deduplicated");
    }

    /// Not every instance populates extensions, so the message is a fallback.
    #[test]
    fn undefined_fields_fall_back_to_parsing_the_message() {
        let error = GraphQlError {
            message: "Field 'approvalsLeft' doesn't exist on type 'MergeRequest'".into(),
            ..GraphQlError::default()
        };
        assert_eq!(unknown_fields(&[error]), ["approvalsLeft"]);
    }

    #[test]
    fn an_undefined_field_without_a_usable_name_is_not_reported() {
        let error = GraphQlError {
            message: "something is wrong".into(),
            code: Some(CODE_UNDEFINED_FIELD.into()),
            ..GraphQlError::default()
        };
        assert!(unknown_fields(&[error]).is_empty());
    }

    /// Validation precedes cost analysis, so an unknown field masks a complexity
    /// rejection. Classifying the other way round would leave the field fallback never
    /// firing on an instance that has both problems.
    #[test]
    fn unknown_fields_take_precedence_over_complexity() {
        let errors = [
            GraphQlError::new("Query has complexity of 318, which exceeds max complexity of 250"),
            undefined("approvalsLeft"),
        ];

        match classify(&errors, false) {
            Some(Error::UnknownField { fields }) => assert_eq!(fields, ["approvalsLeft"]),
            other => panic!("validation must win, got {other:?}"),
        }
    }

    #[test]
    fn complexity_rejection_is_recognised_with_its_limit() {
        let errors = [GraphQlError::new(
            "Query has complexity of 318, which exceeds max complexity of 250",
        )];

        match classify(&errors, false) {
            Some(Error::LimitExceeded { kind, limit }) => {
                assert_eq!(kind, LimitKind::Complexity);
                assert_eq!(limit, Some(250), "the limit, not the query's own score");
            }
            other => panic!("expected LimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn depth_rejection_is_distinguished_from_complexity() {
        let errors = [GraphQlError::new(
            "Query has depth of 20, which exceeds max depth of 15",
        )];

        match classify(&errors, false) {
            Some(Error::LimitExceeded { kind, limit }) => {
                assert_eq!(kind, LimitKind::Depth);
                assert_eq!(limit, Some(15));
            }
            other => panic!("expected LimitExceeded, got {other:?}"),
        }
    }

    /// A message mentioning complexity without rejecting anything is not a limit error.
    #[test]
    fn unrelated_messages_are_not_mistaken_for_limits() {
        let errors = [GraphQlError::new("complexity is a feature of this schema")];
        assert!(matches!(
            classify(&errors, false),
            Some(Error::GraphQl { .. })
        ));
    }

    /// Data plus errors means render what arrived and mark the tab.
    #[test]
    fn data_alongside_errors_is_partial_not_a_failure() {
        let errors = [GraphQlError::new("one field resolver failed")];

        match classify(&errors, true) {
            Some(Error::GraphQlPartial { errors }) => assert_eq!(errors.len(), 1),
            other => panic!("expected GraphQlPartial, got {other:?}"),
        }
    }

    #[test]
    fn errors_without_data_are_a_total_failure() {
        let errors = [GraphQlError::new("nope")];
        assert!(matches!(
            classify(&errors, false),
            Some(Error::GraphQl { .. })
        ));
    }

    /// 2s, 4s, 8s, … capped at the refresh interval.
    #[test]
    fn backoff_doubles_from_two_seconds() {
        let mut backoff = Backoff::new(Duration::from_secs(300));

        assert_eq!(backoff.record_failure(None), Duration::from_secs(2));
        assert_eq!(backoff.record_failure(None), Duration::from_secs(4));
        assert_eq!(backoff.record_failure(None), Duration::from_secs(8));
        assert_eq!(backoff.record_failure(None), Duration::from_secs(16));
        assert_eq!(backoff.attempt(), 4);
    }

    /// Backing off past the refresh interval would leave a once-failed filter refreshing
    /// less often than a healthy one, indefinitely.
    #[test]
    fn backoff_is_capped_at_the_refresh_interval() {
        let cap = Duration::from_secs(60);
        let mut backoff = Backoff::new(cap);

        for _ in 0..20 {
            assert!(backoff.record_failure(None) <= cap);
        }
        assert_eq!(backoff.record_failure(None), cap);
    }

    /// A filter failing for days must not overflow into a tiny delay.
    #[test]
    fn backoff_does_not_overflow_after_many_failures() {
        let cap = Duration::from_secs(300);
        let mut backoff = Backoff::new(cap);

        for _ in 0..10_000 {
            backoff.record_failure(None);
        }
        assert_eq!(backoff.record_failure(None), cap);
    }

    /// The server knows when its rate-limit window closes; we do not.
    #[test]
    fn a_server_retry_after_overrides_the_computed_delay() {
        let mut backoff = Backoff::new(Duration::from_secs(300));
        backoff.record_failure(None);
        backoff.record_failure(None);

        let server = Duration::from_secs(45);
        assert_eq!(backoff.record_failure(Some(server)), server);
        assert_eq!(backoff.attempt(), 3, "still counts as a failure");
    }

    /// A server value above the cap is still honoured — the cap protects the user from
    /// our own arithmetic, not from the server's instruction.
    #[test]
    fn a_server_retry_after_may_exceed_the_cap() {
        let mut backoff = Backoff::new(Duration::from_secs(30));
        let server = Duration::from_secs(120);
        assert_eq!(backoff.record_failure(Some(server)), server);
    }

    #[test]
    fn success_clears_the_failure_streak() {
        let mut backoff = Backoff::new(Duration::from_secs(300));
        backoff.record_failure(None);
        backoff.record_failure(None);
        assert!(backoff.is_failing());

        backoff.reset();
        assert!(!backoff.is_failing());
        assert_eq!(backoff.attempt(), 0);
        assert_eq!(
            backoff.record_failure(None),
            Duration::from_secs(2),
            "the next failure starts over"
        );
    }

    /// A cap below the base delay would otherwise produce a retry storm.
    #[test]
    fn an_absurdly_small_cap_is_raised_to_the_base_delay() {
        let mut backoff = Backoff::new(Duration::from_millis(1));
        assert_eq!(backoff.record_failure(None), Duration::from_secs(2));
    }
}
