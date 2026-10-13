//! A merge request's unified diff, for the terminal viewer.
//!
//! GraphQL exposes diff statistics but not the diff itself, so this is the one place mrq
//! uses the REST API. `raw_diffs` returns exactly what `git diff` prints, but only on
//! GitLab 17.9 and later; older instances answer 404 and the diff is rebuilt from the
//! paginated `diffs` endpoint instead.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::gitlab::client::Client;

/// How many pages of `diffs` to read before giving up. At 100 files a page, a merge
/// request beyond this is not one anybody reads in a pager.
const MAX_PAGES: u32 = 50;

/// Fetch the unified diff of one merge request.
pub async fn fetch(client: &Client, project_path: &str, iid: &str) -> Result<String> {
    let raw = client
        .rest_get(
            &["projects", project_path, "merge_requests", iid, "raw_diffs"],
            &[],
        )
        .await;
    match raw {
        Ok((_, body)) => Ok(body),
        // Also what a missing merge request returns; the fallback then fails the same
        // way, which is the error worth reporting.
        Err(Error::Http { status: 404 }) => rebuild(client, project_path, iid).await,
        Err(error) => Err(error),
    }
}

/// One file of the `diffs` endpoint.
#[derive(Debug, Deserialize)]
struct FileDiff {
    old_path: String,
    new_path: String,
    #[serde(default)]
    a_mode: String,
    #[serde(default)]
    b_mode: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    renamed_file: bool,
    #[serde(default)]
    deleted_file: bool,
    #[serde(default)]
    diff: String,
}

/// Rebuild a `git diff` from the per-file hunks of the `diffs` endpoint.
async fn rebuild(client: &Client, project_path: &str, iid: &str) -> Result<String> {
    let mut out = String::new();
    let mut page = 1;
    loop {
        let page_param = page.to_string();
        let (headers, body) = client
            .rest_get(
                &["projects", project_path, "merge_requests", iid, "diffs"],
                &[("per_page", "100"), ("page", &page_param)],
            )
            .await?;
        let files: Vec<FileDiff> = serde_json::from_str(&body)
            .map_err(|e| Error::Other(format!("GitLab returned diffs that are not JSON: {e}")))?;
        for file in &files {
            render(&mut out, file);
        }

        let next = headers
            .get("x-next-page")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u32>().ok());
        match next {
            Some(next) if next > page && next <= MAX_PAGES => page = next,
            _ => break,
        }
    }
    Ok(out)
}

/// Append one file in `git diff` form.
fn render(out: &mut String, file: &FileDiff) {
    use std::fmt::Write as _;

    let _ = writeln!(out, "diff --git a/{} b/{}", file.old_path, file.new_path);
    if file.new_file {
        let _ = writeln!(out, "new file mode {}", file.b_mode);
    } else if file.deleted_file {
        let _ = writeln!(out, "deleted file mode {}", file.a_mode);
    } else if file.a_mode != file.b_mode && !file.a_mode.is_empty() && !file.b_mode.is_empty() {
        let _ = writeln!(out, "old mode {}\nnew mode {}", file.a_mode, file.b_mode);
    }
    if file.renamed_file {
        let _ = writeln!(
            out,
            "rename from {}\nrename to {}",
            file.old_path, file.new_path
        );
    }
    // A binary, collapsed or pure-rename file has no hunks, and git prints no `---`/`+++`
    // pair for it either.
    if file.diff.is_empty() {
        return;
    }
    let old = if file.new_file {
        "/dev/null".to_owned()
    } else {
        format!("a/{}", file.old_path)
    };
    let new = if file.deleted_file {
        "/dev/null".to_owned()
    } else {
        format!("b/{}", file.new_path)
    };
    let _ = writeln!(out, "--- {old}\n+++ {new}");
    out.push_str(&file.diff);
    if !file.diff.ends_with('\n') {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Gitlab;
    use crate::config::token::{TokenEnv, resolve};
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
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

    const RAW: &str = "/api/v4/projects/group%2Fsub%2Fproject/merge_requests/7/raw_diffs";
    const DIFFS: &str = "/api/v4/projects/group%2Fsub%2Fproject/merge_requests/7/diffs";

    #[tokio::test]
    async fn the_raw_diff_is_returned_as_is() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(RAW))
            .and(header("authorization", "Bearer glpat-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("diff --git a/x b/x\n"))
            .expect(1)
            .mount(&server)
            .await;

        let diff = fetch(&client_for(&server), "group/sub/project", "7")
            .await
            .unwrap();
        assert_eq!(diff, "diff --git a/x b/x\n");
    }

    /// GitLab before 17.9 has no `raw_diffs`; the paginated endpoint is stitched into the
    /// same shape instead.
    #[tokio::test]
    async fn an_older_instance_falls_back_to_the_paginated_diffs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(RAW))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(DIFFS))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-next-page", "2")
                    .set_body_json(json!([
                        {
                            "old_path": "src/a.rs", "new_path": "src/a.rs",
                            "a_mode": "100644", "b_mode": "100644",
                            "new_file": false, "renamed_file": false, "deleted_file": false,
                            "diff": "@@ -1 +1 @@\n-old\n+new\n"
                        },
                        {
                            "old_path": "new.txt", "new_path": "new.txt",
                            "a_mode": "0", "b_mode": "100644",
                            "new_file": true, "renamed_file": false, "deleted_file": false,
                            "diff": "@@ -0,0 +1 @@\n+hello"
                        }
                    ])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(DIFFS))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-next-page", "")
                    .set_body_json(json!([
                        {
                            "old_path": "gone.txt", "new_path": "gone.txt",
                            "a_mode": "100644", "b_mode": "0",
                            "new_file": false, "renamed_file": false, "deleted_file": true,
                            "diff": "@@ -1 +0,0 @@\n-bye\n"
                        },
                        {
                            "old_path": "before.rs", "new_path": "after.rs",
                            "a_mode": "100644", "b_mode": "100644",
                            "new_file": false, "renamed_file": true, "deleted_file": false,
                            "diff": ""
                        }
                    ])),
            )
            .mount(&server)
            .await;

        let diff = fetch(&client_for(&server), "group/sub/project", "7")
            .await
            .unwrap();
        assert_eq!(
            diff,
            "diff --git a/src/a.rs b/src/a.rs\n\
             --- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n\
             diff --git a/new.txt b/new.txt\nnew file mode 100644\n\
             --- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n\
             diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n\
             --- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n\
             diff --git a/before.rs b/after.rs\nrename from before.rs\nrename to after.rs\n"
        );
    }

    #[tokio::test]
    async fn a_rejected_token_is_an_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let error = fetch(&client_for(&server), "group/sub/project", "7")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Unauthorized { status: 401, .. }));
    }

    #[tokio::test]
    async fn a_missing_merge_request_reports_the_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let error = fetch(&client_for(&server), "group/sub/project", "7")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Http { status: 404 }));
    }
}
