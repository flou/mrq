//! A merge request's discussions, fetched on demand for the comments popup.
//!
//! The list query only carries the counts (`resolvableDiscussionsCount` and friends):
//! pulling every thread of every row into it would multiply the query's cost for data
//! that is looked at one merge request at a time. This is the separate, single-MR
//! request that runs when the popup opens.
//!
//! GitLab's vocabulary: a *note* is the atomic record, a *discussion* is a thread of
//! notes. System notes ("added 1 commit", "approved this merge request") are dropped
//! here — the popup is for what people said.

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::gitlab::client::Client;
use crate::gitlab::model::User;
use crate::gitlab::wire::{Connection, WireUser};

/// Threads requested per merge request, and notes requested per thread.
///
/// Kept well under the connection maximum of 100: the two nest, and the product is what
/// the instance's complexity analysis charges for.
const DISCUSSIONS_CAP: u32 = 50;
const NOTES_CAP: u32 = 30;

/// One request for one merge request's threads.
pub fn query() -> String {
    format!(
        "\
query MrqDiscussions($path: ID!, $iid: String!) {{
  project(fullPath: $path) {{
    mergeRequest(iid: $iid) {{
      discussions(first: {DISCUSSIONS_CAP}) {{
        nodes {{
          resolvable
          resolved
          notes(first: {NOTES_CAP}) {{
            nodes {{
              system
              body
              createdAt
              author {{ username name }}
              position {{ filePath newLine oldLine }}
            }}
          }}
        }}
      }}
    }}
  }}
}}
"
    )
}

/// A thread of comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discussion {
    /// Only resolvable threads (diff comments, "Start a thread") have a resolved state.
    pub resolvable: bool,
    pub resolved: bool,
    pub notes: Vec<Note>,
}

impl Discussion {
    /// Whether this thread still needs someone's attention.
    pub const fn is_unresolved(&self) -> bool {
        self.resolvable && !self.resolved
    }
}

/// One comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub author: User,
    pub body: String,
    pub created_at: Option<Timestamp>,
    /// Where in the diff the comment sits; `None` for a general comment.
    pub position: Option<Position>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub path: String,
    pub line: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Data {
    project: Option<WireProject>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireProject {
    merge_request: Option<WireMergeRequest>,
}

#[derive(Debug, Deserialize)]
struct WireMergeRequest {
    #[serde(default)]
    discussions: Connection<WireDiscussion>,
}

#[derive(Debug, Deserialize)]
struct WireDiscussion {
    #[serde(default)]
    resolvable: bool,
    #[serde(default)]
    resolved: bool,
    #[serde(default)]
    notes: Connection<WireNote>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireNote {
    #[serde(default)]
    system: bool,
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: Option<Timestamp>,
    author: Option<WireUser>,
    position: Option<WirePosition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePosition {
    file_path: Option<String>,
    new_line: Option<u32>,
    old_line: Option<u32>,
}

impl From<WireNote> for Note {
    fn from(note: WireNote) -> Self {
        let position = note.position.and_then(|p| {
            Some(Position {
                path: p.file_path?,
                line: p.new_line.or(p.old_line),
            })
        });
        Self {
            // A deleted account comes back as a null author.
            author: note.author.map_or_else(|| User::new("ghost"), User::from),
            body: note.body,
            created_at: note.created_at,
            position,
        }
    }
}

/// Drop system notes and the threads they leave empty, and put the threads that still
/// need attention first. The sort is stable, so GitLab's order holds within each group.
fn convert(data: Data) -> Option<Vec<Discussion>> {
    let mr = data.project?.merge_request?;
    let mut discussions: Vec<Discussion> = mr
        .discussions
        .nodes
        .into_iter()
        .filter_map(|d| {
            let notes: Vec<Note> = d
                .notes
                .nodes
                .into_iter()
                .filter(|n| !n.system)
                .map(Note::from)
                .collect();
            (!notes.is_empty()).then_some(Discussion {
                resolvable: d.resolvable,
                resolved: d.resolved,
                notes,
            })
        })
        .collect();
    discussions.sort_by_key(|d| !d.is_unresolved());
    Some(discussions)
}

/// Fetch the threads of one merge request.
pub async fn fetch(client: &Client, project_path: &str, iid: &str) -> Result<Vec<Discussion>> {
    let variables = json!({ "path": project_path, "iid": iid });
    let response = client.execute::<Data, _>(&query(), &variables).await?;

    if let Some(failure) = response.failure() {
        return Err(failure);
    }

    response
        .data
        .and_then(convert)
        .ok_or_else(|| Error::Other(format!("merge request !{iid} of {project_path} not found")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Gitlab;
    use crate::config::token::{TokenEnv, resolve};
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

    fn body() -> serde_json::Value {
        json!({"data": {"project": {"mergeRequest": {"discussions": {"nodes": [
            {
                "resolvable": true, "resolved": true,
                "notes": {"nodes": [{
                    "system": false, "body": "Looks fine",
                    "createdAt": "2026-09-10T09:00:00Z",
                    "author": {"username": "jdoe", "name": "Jane Doe"},
                    "position": null
                }]}
            },
            {
                "resolvable": false, "resolved": false,
                "notes": {"nodes": [{
                    "system": true, "body": "added 1 commit",
                    "createdAt": "2026-09-10T10:00:00Z",
                    "author": {"username": "jdoe", "name": null},
                    "position": null
                }]}
            },
            {
                "resolvable": true, "resolved": false,
                "notes": {"nodes": [
                    {
                        "system": false, "body": "Please rename this",
                        "createdAt": "2026-09-11T08:00:00Z",
                        "author": {"username": "bwayne", "name": null},
                        "position": {"filePath": "src/lib.rs", "newLine": 42, "oldLine": null}
                    },
                    {
                        "system": false, "body": "Done",
                        "createdAt": "2026-09-11T09:00:00Z",
                        "author": null,
                        "position": {"filePath": "src/lib.rs", "newLine": null, "oldLine": null}
                    }
                ]}
            }
        ]}}}}})
    }

    #[tokio::test]
    async fn system_notes_are_dropped_and_unresolved_threads_come_first() {
        let server = responding(body()).await;

        let found = fetch(&client_for(&server), "acme/app", "7").await.unwrap();

        assert_eq!(found.len(), 2, "the system-only thread disappears");
        assert!(found[0].is_unresolved(), "unresolved first: {found:?}");
        assert!(!found[1].is_unresolved());
        assert_eq!(found[0].notes.len(), 2);
        assert_eq!(found[1].notes[0].body, "Looks fine");
    }

    #[tokio::test]
    async fn positions_and_missing_authors_are_mapped() {
        let server = responding(body()).await;

        let found = fetch(&client_for(&server), "acme/app", "7").await.unwrap();
        let thread = &found[0];

        assert_eq!(
            thread.notes[0].position,
            Some(Position {
                path: "src/lib.rs".into(),
                line: Some(42)
            })
        );
        assert_eq!(thread.notes[1].author.username, "ghost");
        assert_eq!(
            thread.notes[1].position.as_ref().map(|p| p.line),
            Some(None)
        );
        assert_eq!(found[1].notes[0].position, None);
    }

    #[tokio::test]
    async fn an_unknown_merge_request_is_an_error() {
        let server = responding(json!({"data": {"project": {"mergeRequest": null}}})).await;

        let err = fetch(&client_for(&server), "acme/app", "7")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("!7"), "{err}");
    }

    #[tokio::test]
    async fn graphql_errors_fail_the_fetch() {
        let server = responding(json!({"errors": [{"message": "nope"}]})).await;

        let err = fetch(&client_for(&server), "acme/app", "7")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::GraphQl { .. }), "{err:?}");
    }

    #[test]
    fn the_query_asks_for_the_variables_it_declares() {
        let q = query();
        assert!(q.contains("project(fullPath: $path)"));
        assert!(q.contains("mergeRequest(iid: $iid)"));
        assert!(q.contains("discussions(first: 50)"));
    }
}
