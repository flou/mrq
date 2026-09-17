//! The startup identity probe.
//!
//! Runs once, before any filter is fetched, for one reason that cannot be deferred: the
//! ASSIGNED and APRV columns are defined relative to the current user, so no row can
//! be rendered correctly until the username is known. Everything else it returns — the
//! instance version, the display name — is incidental.
//!
//! It is also the first request of the session, which makes it where a wrong token is
//! discovered. Failing here is fatal and exits 2, because there is nothing to show and
//! no reason to let the TUI start and then sit empty.

use crate::error::{Error, Phase, Recovery, Result};
use crate::gitlab::client::Client;
use crate::gitlab::query::CURRENT_USER_QUERY;
use crate::gitlab::wire::CurrentUserData;

/// Who we are, and what we are talking to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub username: String,
    pub name: Option<String>,
    /// `None` on instances that restrict the `metadata` field.
    pub version: Option<String>,
}

/// Ask GitLab who the token belongs to.
pub async fn identify(client: &Client) -> Result<Identity> {
    let response = client
        .execute::<CurrentUserData, _>(CURRENT_USER_QUERY, &serde_json::json!({}))
        .await?;

    if let Some(failure) = response.failure() {
        // A probe that half-worked is still a probe that did not tell us who we are, so
        // a partial response is treated like any other failure here.
        return Err(failure);
    }

    let data = response
        .data
        .ok_or_else(|| Error::Other("GitLab returned no data for the identity probe".to_owned()))?;

    // A token that authenticates but resolves to no user is what a revoked or
    // project-scoped token looks like. It would otherwise surface much later as every
    // row reading "not assigned to me", which is a far harder thing to diagnose.
    let user = data.current_user.ok_or_else(|| client.no_current_user())?;

    let version = data.metadata.and_then(|m| m.version);
    Ok(Identity {
        username: user.username,
        name: user.name,
        version,
    })
}

/// Run the probe and log what it found.
///
/// The version is logged once, here, rather than attached to later diagnostics: it never
/// changes during a session, and repeating it on every fetch would bury the lines that
/// do carry new information.
pub async fn identify_and_log(client: &Client) -> Result<Identity> {
    let identity = identify(client).await?;
    tracing::info!(
        username = %identity.username,
        version = identity.version.as_deref().unwrap_or("unknown"),
        endpoint = client.endpoint(),
        "authenticated"
    );
    Ok(identity)
}

/// Whether a probe failure should stop the program.
///
/// Always true in practice — the probe runs at startup — but expressed through the
/// shared recovery policy so it cannot drift from what the retry policy says about a 401.
pub const fn is_fatal(error: &Error) -> bool {
    matches!(error.recovery(Phase::Startup), Recovery::Fatal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Gitlab;
    use crate::config::token::{TokenEnv, resolve};
    use crate::error::EXIT_CONFIG;
    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    async fn responding(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    /// The probe asks for exactly what it needs and nothing more, so it cannot be the
    /// thing that delays the first paint.
    #[test]
    fn the_probe_query_is_small() {
        assert!(CURRENT_USER_QUERY.contains("currentUser"));
        assert!(CURRENT_USER_QUERY.contains("username"));
        assert!(CURRENT_USER_QUERY.contains("metadata"));
        assert!(
            !CURRENT_USER_QUERY.contains("mergeRequests"),
            "the probe must not fetch merge requests"
        );
        assert!(CURRENT_USER_QUERY.len() < 200, "one small document");
    }

    /// Some instances restrict `metadata`; that is not a reason to refuse to start.
    #[tokio::test]
    async fn a_missing_version_is_tolerated() {
        let server = responding(json!({"data": {
            "currentUser": {"username": "someone"}
        }}))
        .await;

        let identity = identify(&client_for(&server)).await.unwrap();
        assert_eq!(identity.username, "someone");
        assert_eq!(identity.version, None);
        assert_eq!(identity.name, None);
    }

    /// A 401 at startup is fatal and exits 2, with a message naming what
    /// to check.
    #[tokio::test]
    async fn an_unauthorized_probe_is_fatal_with_exit_code_two() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let err = identify(&client_for(&server)).await.unwrap_err();

        assert!(is_fatal(&err));
        assert_eq!(err.exit_code(), EXIT_CONFIG);
        assert!(err.to_string().contains("read_api"), "{err}");
    }

    #[tokio::test]
    async fn a_forbidden_probe_is_also_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let err = identify(&client_for(&server)).await.unwrap_err();
        assert!(is_fatal(&err));
        assert_eq!(err.exit_code(), EXIT_CONFIG);
    }

    /// A token can authenticate and still resolve to no user — a revoked or
    /// project-scoped token does exactly that. Left unhandled it surfaces much later as
    /// every row reading "not assigned to me".
    #[tokio::test]
    async fn a_null_current_user_is_treated_as_an_auth_failure() {
        let server = responding(json!({"data": {"currentUser": null}})).await;

        let err = identify(&client_for(&server)).await.unwrap_err();
        assert!(matches!(err, Error::NoCurrentUser { .. }), "{err:?}");
        assert_eq!(err.exit_code(), EXIT_CONFIG);
        // This path bypasses `classify_status`, so it is the one that would silently
        // regress to a context-free message if `client.no_current_user` were dropped here.
        assert!(err.to_string().contains(&server.uri()), "{err}");
        assert!(
            !err.to_string().contains("HTTP"),
            "the instance returned 200, not a rejection: {err}"
        );
    }

    #[tokio::test]
    async fn graphql_errors_fail_the_probe() {
        let server = responding(json!({"errors": [{"message": "nope"}]})).await;

        let err = identify(&client_for(&server)).await.unwrap_err();
        assert!(matches!(err, Error::GraphQl { .. }), "{err:?}");
    }

    /// A transport failure at startup is fatal too: there is no cached identity to fall
    /// back on, and the username is required before any row can render.
    #[tokio::test]
    async fn an_unreachable_instance_is_fatal_at_startup() {
        let gitlab = Gitlab {
            url: "http://127.0.0.1:1".into(),
            token: Some("glpat-test".into()),
            timeout_secs: 2,
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        let client = Client::new(&gitlab, token).unwrap();

        let err = identify(&client).await.unwrap_err();
        assert!(is_fatal(&err), "{err:?}");
        assert_eq!(
            err.exit_code(),
            crate::error::EXIT_FAILURE,
            "unreachable is a runtime failure, not a config one"
        );
    }

    /// The username drives the ASSIGNED and APRV columns, so the probe's whole
    /// purpose is to make this computation possible.
    #[tokio::test]
    async fn the_username_drives_the_derived_flags() {
        use crate::gitlab::model::fixtures::mr;

        let server = responding(json!({"data": {"currentUser": {"username": "asmith"}}})).await;
        let identity = identify(&client_for(&server)).await.unwrap();

        let mut merge_request = mr("1", "asmith");
        merge_request.recompute_derived(&identity.username);
        assert!(merge_request.authored_by_me());
    }
}
