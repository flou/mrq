//! The GraphQL transport: one client, one place that builds a request.
//!
//! Everything that talks to GitLab goes through here, so the auth header,
//! the timeout, the user agent and the endpoint are decided once instead of at each call
//! site.
//!
//! # What this layer does and does not decide
//!
//! It classifies *transport* failures — unreachable, timed out, 401, 429, 5xx — because
//! those are visible only here, in the status line and the headers. It does **not**
//! interpret the GraphQL `errors` array: a response can carry both `data` and `errors`,
//! and deciding whether that is a degradation or a failure needs to know what was being
//! asked for. [`Response`] therefore hands both back and lets the caller classify it.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::schema::Gitlab;
use crate::config::token::Token;
use crate::error::{Error, GraphQlError, Result};

/// A GraphQL request body.
#[derive(Debug, Serialize)]
struct RequestBody<'a, V> {
    query: &'a str,
    variables: V,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_name: Option<&'a str>,
}

/// A GraphQL response envelope.
///
/// Both fields are optional and both can be present at once: GitLab returns partial data
/// alongside errors when only part of a query failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response<T> {
    pub data: Option<T>,
    pub errors: Vec<GraphQlError>,
}

impl<T> Response<T> {
    /// Whether the server reported anything wrong.
    #[cfg(test)]
    const fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// The classified failure, if the response carries one.
    ///
    /// A GraphQL failure arrives as HTTP 200 with an `errors` array, so this is the only
    /// place the distinction between "worked", "degraded" and "failed" can be drawn.
    pub fn failure(&self) -> Option<Error> {
        super::error::classify(&self.errors, self.data.is_some())
    }

    /// Whether some data came back despite errors — "GraphQL partial errors", which the
    /// project requires to be rendered rather than discarded.
    #[cfg(test)]
    const fn is_partial(&self) -> bool {
        self.data.is_some() && self.has_errors()
    }
}

/// The wire shape, kept private so the rest of the program sees only [`Response`].
#[derive(Debug, serde::Deserialize)]
struct WireResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<WireError>,
}

#[derive(Debug, serde::Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    path: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    extensions: Option<WireErrorExtensions>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireErrorExtensions {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    field_name: Option<String>,
}

impl From<WireError> for GraphQlError {
    fn from(e: WireError) -> Self {
        let (code, field) = match e.extensions {
            Some(x) => (x.code, x.field_name),
            None => (None, None),
        };
        Self {
            message: e.message,
            code,
            field,
            // GraphQL paths mix field names and array indices; a dotted string is what
            // the log and the status bar can actually show.
            path: e.path.map(|segments| {
                segments
                    .iter()
                    .map(|s| match s {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(".")
            }),
        }
    }
}

#[derive(Debug)]
struct Inner {
    http: reqwest::Client,
    endpoint: String,
    /// The instance root, in the shape the user wrote into `gitlab.url` — named in the
    /// `Unauthorized` message so a token valid for a different instance is distinguishable
    /// from a bad one.
    instance: String,
    token: Token,
}

/// A GitLab GraphQL client.
///
/// Cheap to clone — one `Arc` bump — because one fetch task runs per filter and they
/// all share this.
#[derive(Debug, Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

/// The `User-Agent` sent with every request, so instance operators can identify the
/// traffic.
pub fn user_agent() -> String {
    format!("mrq/{}", env!("CARGO_PKG_VERSION"))
}

/// The instance root from a configured URL.
///
/// Users write it with and without a trailing slash, and some paste a URL that already
/// ends in `/api/graphql`. All three should normalize to the same root rather than
/// producing a 404 whose cause is invisible.
pub fn instance_root(instance_url: &str) -> String {
    let trimmed = instance_url.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/api/graphql")
        .unwrap_or(trimmed)
        .to_owned()
}

/// Build the GraphQL endpoint from a configured instance URL.
pub fn graphql_endpoint(instance_url: &str) -> String {
    format!("{}/api/graphql", instance_root(instance_url))
}

impl Client {
    pub fn new(config: &Gitlab, token: Token) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .user_agent(user_agent())
            .gzip(true)
            .build()
            .map_err(|e| Error::Network(Box::new(e)))?;

        Ok(Self {
            inner: Arc::new(Inner {
                http,
                endpoint: graphql_endpoint(&config.url),
                instance: instance_root(&config.url),
                token,
            }),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    /// The auth failure for this client. A valid token sent to the wrong instance is
    /// otherwise indistinguishable from a bad one.
    pub fn unauthorized(&self, status: u16) -> Error {
        Error::Unauthorized {
            status,
            instance: self.inner.instance.clone(),
            token_source: self.inner.token.source(),
        }
    }

    /// The failure for a token that authenticated (HTTP 200) but resolved to no user.
    pub fn no_current_user(&self) -> Error {
        Error::NoCurrentUser {
            instance: self.inner.instance.clone(),
            token_source: self.inner.token.source(),
        }
    }

    /// Execute one GraphQL document.
    ///
    /// Returns the envelope on any HTTP 2xx, including one carrying errors. A non-2xx is
    /// classified here because the status and headers are only visible at this layer.
    pub async fn execute<T, V>(&self, query: &str, variables: &V) -> Result<Response<T>>
    where
        T: DeserializeOwned,
        V: Serialize,
    {
        let body = RequestBody {
            query,
            variables,
            operation_name: None,
        };

        let response = self
            .inner
            .http
            .post(&self.inner.endpoint)
            // Bearer rather than the PRIVATE-TOKEN header: it works for both personal
            // access tokens and OAuth tokens, so the auth path stays the same if OAuth
            // is ever added.
            .bearer_auth(self.inner.token.expose())
            .json(&body)
            .send()
            .await
            .map_err(classify_transport)?;

        let status = response.status();
        if !status.is_success() {
            return Err(self.classify_status(&response));
        }

        let wire: WireResponse<T> = response.json().await.map_err(|e| {
            // A 2xx that is not GraphQL JSON almost always means a proxy or SSO login
            // page intercepted the request, which is worth saying outright.
            Error::Network(Box::new(std::io::Error::other(format!(
                "GitLab returned a {status} response that is not GraphQL JSON \
                 (is the URL behind an SSO proxy?): {e}"
            ))))
        })?;

        Ok(Response {
            data: wire.data,
            errors: wire.errors.into_iter().map(GraphQlError::from).collect(),
        })
    }

    fn classify_status(&self, response: &reqwest::Response) -> Error {
        let status = response.status().as_u16();
        let retry_after = parse_retry_after(response);

        match status {
            401 | 403 => self.unauthorized(status),
            429 => Error::RateLimited { retry_after },
            _ => {
                // A Retry-After on any status is an instruction worth honouring; GitLab
                // sends one with 503 during maintenance.
                if let Some(retry_after) = retry_after {
                    Error::RateLimited {
                        retry_after: Some(retry_after),
                    }
                } else {
                    Error::Http { status }
                }
            }
        }
    }
}

fn classify_transport(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        // Surfaced distinctly because the fix differs: a timeout usually means the
        // filter is too broad, not that the instance is down.
        Error::Network(Box::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "request timed out; consider raising gitlab.timeout_secs or narrowing the filter",
        )))
    } else {
        Error::Network(Box::new(e))
    }
}

/// Parse `Retry-After`, which is either a delay in seconds or an HTTP date.
///
/// Only the numeric form is honoured. The date form would need the server's clock to
/// agree with ours, and a skewed clock turns a 30-second pause into an hour-long one.
fn parse_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let raw = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let secs: u64 = raw.to_str().ok()?.trim().parse().ok()?;
    // Cap it: a server asking us to wait a day is a bug, and honouring it looks like a
    // hang with no explanation.
    Some(Duration::from_secs(secs.min(3600)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Gitlab;
    use serde::Deserialize;
    use serde_json::json;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Me {
        username: String,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Data {
        me: Option<Me>,
    }

    fn client_for(server: &MockServer) -> Client {
        let gitlab = Gitlab {
            url: server.uri(),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        Client::new(&gitlab, test_token()).unwrap()
    }

    fn test_token() -> Token {
        // Resolution is covered in config::token; here we only need a value to send.
        let gitlab = Gitlab {
            token: Some("glpat-test".into()),
            ..Gitlab::default()
        };
        crate::config::token::resolve(&gitlab, &crate::config::token::TokenEnv::default(), None)
            .unwrap()
            .token
    }

    /// The documented endpoint, method and headers.
    #[tokio::test]
    async fn requests_carry_the_documented_method_path_and_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .and(header("authorization", "Bearer glpat-test"))
            .and(header("content-type", "application/json"))
            .and(header_exists("user-agent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": {"me": {"username": "u"}}})),
            )
            .mount(&server)
            .await;

        let got: Response<Data> = client_for(&server)
            .execute("query { me }", &json!({}))
            .await
            .unwrap();
        assert_eq!(got.data.unwrap().me.unwrap().username, "u");
    }

    #[test]
    fn the_user_agent_identifies_the_version() {
        let ua = user_agent();
        assert!(ua.starts_with("mrq/"), "{ua}");
        assert!(ua.len() > "mrq/".len(), "{ua}");
    }

    /// Users write the instance root in several shapes; all should reach the same place
    /// rather than producing a 404 with no visible cause.
    #[test]
    fn endpoint_construction_tolerates_how_people_write_urls() {
        for input in [
            "https://gitlab.example.com",
            "https://gitlab.example.com/",
            "https://gitlab.example.com///",
            "  https://gitlab.example.com  ",
            "https://gitlab.example.com/api/graphql",
            "https://gitlab.example.com/api/graphql/",
        ] {
            assert_eq!(
                graphql_endpoint(input),
                "https://gitlab.example.com/api/graphql",
                "for input `{input}`"
            );
        }
    }

    /// Self-managed instances are often served from a subpath.
    #[test]
    fn endpoint_construction_preserves_a_subpath() {
        assert_eq!(
            graphql_endpoint("https://example.com/gitlab/"),
            "https://example.com/gitlab/api/graphql"
        );
    }

    /// The root is what the user wrote into `gitlab.url` and is what the `Unauthorized`
    /// message names, so it must strip the derived endpoint back off.
    #[test]
    fn instance_root_strips_the_endpoint_and_trailing_slashes() {
        for input in [
            "https://gitlab.example.com",
            "https://gitlab.example.com/",
            "https://gitlab.example.com///",
            "  https://gitlab.example.com  ",
            "https://gitlab.example.com/api/graphql",
            "https://gitlab.example.com/api/graphql/",
        ] {
            assert_eq!(
                instance_root(input),
                "https://gitlab.example.com",
                "for input `{input}`"
            );
        }
        assert_eq!(
            instance_root("https://example.com/gitlab/"),
            "https://example.com/gitlab",
            "a subpath is preserved"
        );
    }

    /// A response with both data and errors is the "partial" row: the caller must
    /// be able to render what arrived, so the transport hands back both.
    #[tokio::test]
    async fn partial_responses_return_data_and_errors_together() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"me": null},
                "errors": [{"message": "Field failed", "path": ["me", "avatar", 0]}]
            })))
            .mount(&server)
            .await;

        let got: Response<Data> = client_for(&server).execute("q", &json!({})).await.unwrap();

        assert!(got.is_partial());
        assert!(got.has_errors());
        assert_eq!(got.errors.len(), 1);
        assert_eq!(got.errors[0].message, "Field failed");
        assert_eq!(
            got.errors[0].path.as_deref(),
            Some("me.avatar.0"),
            "mixed field/index paths flatten to something printable"
        );
    }

    #[tokio::test]
    async fn an_errors_only_response_is_still_a_successful_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "no"}]
            })))
            .mount(&server)
            .await;

        let got: Response<Data> = client_for(&server).execute("q", &json!({})).await.unwrap();
        assert!(got.data.is_none());
        assert!(got.has_errors());
        assert!(!got.is_partial(), "no data means not partial");
    }

    #[tokio::test]
    async fn auth_failures_are_classified_from_the_status() {
        for status in [401, 403] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;

            let err = client_for(&server)
                .execute::<Data, _>("q", &json!({}))
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::Unauthorized { status: s, .. } if s == status),
                "{err:?}"
            );
            assert!(err.to_string().contains("read_api"), "{err}");
        }
    }

    /// A valid token sent to the wrong instance is otherwise indistinguishable from a
    /// bad one — the message must name both which instance refused it and where it came
    /// from, and must name the root the user wrote, not the derived `/api/graphql` path.
    #[tokio::test]
    async fn an_auth_failure_names_the_instance_and_the_token_source() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let gitlab = Gitlab {
            url: server.uri(),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        let env = crate::config::token::TokenEnv {
            gitlab_token: Some("glpat-secret".into()),
            ..Default::default()
        };
        let token = crate::config::token::resolve(&gitlab, &env, None)
            .unwrap()
            .token;
        let client = Client::new(&gitlab, token).unwrap();

        let err = client
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&server.uri()), "{msg}");
        assert!(msg.contains("$GITLAB_TOKEN"), "{msg}");
        assert!(msg.contains("read_api"), "{msg}");
        assert!(
            !msg.contains("/api/graphql"),
            "names the root, not the endpoint: {msg}"
        );
    }

    /// The guarantee: never render the token itself, not even in the message that
    /// tells you it was rejected.
    #[tokio::test]
    async fn an_auth_failure_never_renders_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let gitlab = Gitlab {
            url: server.uri(),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        let env = crate::config::token::TokenEnv {
            gitlab_token: Some("glpat-secret".into()),
            ..Default::default()
        };
        let token = crate::config::token::resolve(&gitlab, &env, None)
            .unwrap()
            .token;
        let client = Client::new(&gitlab, token).unwrap();

        let err = client
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("glpat-secret"));
    }

    #[tokio::test]
    async fn rate_limiting_honours_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "45"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.retry_after(), Some(Duration::from_secs(45)));
    }

    #[tokio::test]
    async fn a_429_without_the_header_still_classifies_as_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::RateLimited { retry_after: None }),
            "{err:?}"
        );
    }

    /// GitLab sends Retry-After with 503 during maintenance; honouring it beats our own
    /// backoff, which knows nothing about how long the maintenance will last.
    #[tokio::test]
    async fn retry_after_on_a_5xx_is_honoured_too() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "20"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.retry_after(), Some(Duration::from_secs(20)));
    }

    /// An HTTP-date Retry-After is ignored rather than trusted: it needs the server's
    /// clock to agree with ours, and skew turns a short pause into a long one.
    #[tokio::test]
    async fn a_date_form_retry_after_is_ignored() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"),
            )
            .mount(&server)
            .await;

        let err = client_for(&server)
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.retry_after(), None);
    }

    /// A server asking for a day is a bug; honouring it looks like an unexplained hang.
    #[tokio::test]
    async fn an_absurd_retry_after_is_capped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "86400"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.retry_after(), Some(Duration::from_secs(3600)));
    }

    #[tokio::test]
    async fn server_errors_carry_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Http { status: 502 }), "{err:?}");
    }

    /// The realistic corporate failure: an SSO proxy answers 200 with an HTML login page.
    /// "expected value at line 1" would send the user looking in the wrong place.
    #[tokio::test]
    async fn a_non_json_success_body_names_the_likely_cause() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>Sign in</html>"))
            .mount(&server)
            .await;

        let err = client_for(&server)
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("SSO") || msg.contains("proxy"), "{msg}");
    }

    #[tokio::test]
    async fn an_unreachable_instance_is_a_network_error() {
        let gitlab = Gitlab {
            // Reserved for documentation use; never routable.
            url: "http://127.0.0.1:1".into(),
            timeout_secs: 2,
            ..Gitlab::default()
        };
        let client = Client::new(&gitlab, test_token()).unwrap();

        let err = client
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Network(_)), "{err:?}");
        assert_eq!(err.exit_code(), crate::error::EXIT_FAILURE);
    }

    #[tokio::test]
    async fn a_timeout_suggests_what_to_change() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
            .mount(&server)
            .await;

        let gitlab = Gitlab {
            url: server.uri(),
            timeout_secs: 1,
            ..Gitlab::default()
        };
        let client = Client::new(&gitlab, test_token()).unwrap();

        let err = client
            .execute::<Data, _>("q", &json!({}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("timeout_secs") || msg.contains("timed out"),
            "{msg}"
        );
    }

    /// One fetch task runs per filter, all sharing this client.
    #[tokio::test]
    async fn the_client_is_cheap_to_clone_and_usable_concurrently() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data": {"me": {"username": "u"}}})),
            )
            .mount(&server)
            .await;

        let client = client_for(&server);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let client = client.clone();
            tasks.push(tokio::spawn(async move {
                client.execute::<Data, _>("q", &json!({})).await
            }));
        }
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn variables_are_sent_as_json_not_interpolated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_json(json!({
                "query": "query($n: String!) { me }",
                "variables": {"n": "a\" or 1=1"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"me": null}})))
            .mount(&server)
            .await;

        let got: Result<Response<Data>> = client_for(&server)
            .execute("query($n: String!) { me }", &json!({"n": "a\" or 1=1"}))
            .await;
        assert!(
            got.is_ok(),
            "quoting in a variable must not break the request"
        );
    }
}
