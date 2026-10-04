//! The "Changes (git)" handler bodies shared by agent sessions and terminal
//! panes: the changed-file list, one file's patch, and discard.
//!
//! Both planes ask git directly in a checkout, so the answers are the same;
//! the route files only differ in how they find that checkout and in what
//! they answer when the thing that names it is gone. Each body takes the
//! checkout already resolved -- `None` meaning "not in a repository".

use std::path::Path;

use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::platform::git;
use crate::terminal::routes::git_error;
use crate::{api_error, content_envelope, ApiResult};

#[derive(Debug, Deserialize)]
pub struct VcsFilesQuery {
    /// `working` (default) or `branch`.
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VcsFileQuery {
    /// `working` (default) or `branch`.
    pub mode: Option<String>,
    /// Repo-relative.
    pub path: Option<String>,
    /// Unified context lines, clamped to 0..=25; 3 when absent.
    pub context: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct VcsDiscardBody {
    /// Repo-relative.
    pub path: String,
}

/// `mode` for the git routes: `working` when absent.
pub(crate) fn vcs_mode(mode: Option<&str>) -> ApiResult<git::VcsMode> {
    git::VcsMode::parse(mode.unwrap_or("working")).ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_mode",
            "mode must be 'working' or 'branch'",
        )
    })
}

pub(crate) fn not_a_repository() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::NOT_FOUND,
        "not_a_repository",
        "This directory is not in a git repository",
    )
}

fn unknown_path() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::NOT_FOUND,
        "unknown_path",
        "No changed or tracked file has this path",
    )
}

fn outside_repository() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::FORBIDDEN,
        "path_outside_repository",
        "The path leads outside the repository",
    )
}

/// `vcs/files`: outside a checkout this is an ordinary, empty answer with a
/// reason, not an error -- a phone asks it to decide what to show.
pub(crate) async fn files(toplevel: Option<&Path>, mode: git::VcsMode) -> ApiResult<Json<Value>> {
    let Some(toplevel) = toplevel else {
        return Ok(Json(content_envelope(json!({
            "vcs": null,
            "reason": "not_a_repository",
            "mode": mode.as_str(),
            "truncated": false,
            "files": [],
        }))));
    };
    // Boxed, here and below: git's futures are large, and a debug build
    // polling them inline from a handler overflows a test thread's stack.
    let changes = Box::pin(git::changed_files(toplevel, mode))
        .await
        .map_err(git_error)?;
    Ok(Json(content_envelope(changes.to_json())))
}

/// `vcs/file`: one file's patch, or `404 unknown_path` when the path is not
/// a row of the list.
pub(crate) async fn file(
    toplevel: Option<&Path>,
    mode: git::VcsMode,
    query: &VcsFileQuery,
) -> ApiResult<Json<Value>> {
    let toplevel = toplevel.ok_or_else(not_a_repository)?;
    let path = query.path.as_deref().unwrap_or("");
    let context = query
        .context
        .unwrap_or(git::DEFAULT_CONTEXT_LINES)
        .min(git::MAX_CONTEXT_LINES);
    let patch = Box::pin(git::changed_file_patch(toplevel, mode, path, context))
        .await
        .map_err(git_error)?
        .ok_or_else(unknown_path)?;
    Ok(Json(content_envelope(patch.to_json())))
}

/// `vcs/discard`: throws one file's changes away and answers what was done.
/// The caller logs the success with its own scope.
pub(crate) async fn discard(
    toplevel: Option<&Path>,
    path: &str,
) -> ApiResult<(git::DiscardAction, Json<Value>)> {
    let toplevel = toplevel.ok_or_else(not_a_repository)?;
    let action = Box::pin(git::discard(toplevel, path))
        .await
        .map_err(|err| match err {
            git::DiscardError::UnknownPath => unknown_path(),
            git::DiscardError::OutsideRepository => outside_repository(),
            git::DiscardError::RepositoryIsHome => api_error(
                StatusCode::FORBIDDEN,
                "repository_is_home",
                "This repository is the home directory; discard is refused there",
            ),
            git::DiscardError::ListingTruncated => api_error(
                StatusCode::CONFLICT,
                "listing_truncated",
                "git's answer was cut short, so this file cannot be discarded safely",
            ),
            git::DiscardError::Git(err) => git_error(err),
            git::DiscardError::Io(err) if err.kind() == std::io::ErrorKind::NotFound => {
                unknown_path()
            }
            // A directory on the way became a symlink after it was checked.
            git::DiscardError::Io(err)
                if matches!(err.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) =>
            {
                outside_repository()
            }
            git::DiscardError::Io(err) => {
                tracing::warn!(?path, "discard failed: {err}");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "discard_failed",
                    "The file could not be discarded",
                )
            }
        })?;
    Ok((
        action,
        Json(content_envelope(json!({
            "path": path,
            "action": action.as_str(),
        }))),
    ))
}

#[cfg(test)]
mod tests {
    use crate::*;
    use axum::http::StatusCode;
    use serde_json::Value;
    use std::collections::HashMap;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::{VcsDiscardBody, VcsFileQuery, VcsFilesQuery};

    /// A Herdr socket whose `pane.get` reports each known pane's cwd and
    /// answers `pane_not_found`, as Herdr does, for any other id. Every other
    /// method is unknown, which the backend treats as "no extra detail".
    fn fake_herdr(panes: HashMap<&str, Option<PathBuf>>) -> PathBuf {
        let panes: HashMap<String, Option<String>> = panes
            .into_iter()
            .map(|(id, cwd)| (id.to_owned(), cwd.map(|cwd| cwd.to_string_lossy().into())))
            .collect();
        let socket_path = std::env::temp_dir().join(format!(
            "herdr-pane-vcs-{}.sock",
            uuid::Uuid::new_v4().simple()
        ));
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap_or_default();
                let pane_id = request["params"]["pane_id"].as_str().unwrap_or("");
                let response = match (request["method"].as_str(), panes.get(pane_id)) {
                    (Some("pane.get"), Some(cwd)) => json!({
                        "id": request["id"],
                        "result": { "pane": {
                            "pane_id": pane_id,
                            "workspace_id": "w1",
                            "tab_id": "w1:t1",
                            "cwd": cwd,
                        } },
                    }),
                    (Some("pane.get"), None) => json!({
                        "id": request["id"],
                        "error": { "code": "pane_not_found", "message": "pane not found" },
                    }),
                    _ => json!({
                        "id": request["id"],
                        "error": { "code": "method_not_found", "message": "unknown method" },
                    }),
                };
                let mut stream = reader.into_inner();
                let _ = stream.write_all(response.to_string().as_bytes()).await;
                let _ = stream.write_all(b"\n").await;
                let _ = stream.flush().await;
            }
        });
        socket_path
    }

    /// `w1:p1` sits in a subdirectory of a checkout, `w1:p2` in a plain
    /// directory, `w1:p3` reports no cwd and `w1:p4` a deleted one.
    fn pane_state(name: &str) -> (AppState, PathBuf, PathBuf) {
        let (root, repo) = git_test_repo(name);
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let socket = fake_herdr(HashMap::from([
            ("w1:p1", Some(repo.join("src"))),
            ("w1:p2", Some(plain)),
            ("w1:p3", None),
            ("w1:p4", Some(root.join("gone"))),
        ]));
        let mut state = test_state("admin", vec![test_device("d1", "token")]);
        state.config.sessions[0].socket_path = socket.to_string_lossy().into_owned();
        (state, root, repo)
    }

    fn pane(id: &str) -> Path<(String, String)> {
        Path(("default".into(), id.into()))
    }

    fn file_query(mode: Option<&str>, path: &str, context: Option<u32>) -> VcsFileQuery {
        VcsFileQuery {
            mode: mode.map(str::to_string),
            path: Some(path.to_string()),
            context,
        }
    }

    fn code(refusal: &(StatusCode, Json<Value>)) -> Value {
        error_body(refusal)["error"]["code"].clone()
    }

    #[tokio::test]
    async fn pane_vcs_files_and_file_answer_from_the_panes_cwd() {
        let (state, root, repo) = pane_state("pane-vcs-files");
        // Boxed: a debug build's test future holding every call inline
        // overflows the test thread's stack.
        let files = |id: &str, mode: Option<&str>| {
            Box::pin(pane_vcs_files(
                State(state.clone()),
                pane(id),
                Query(VcsFilesQuery {
                    mode: mode.map(str::to_string),
                }),
                bearer_headers("token"),
            ))
        };

        let Json(answer) = files("w1:p1", None).await.expect("lists");
        assert_eq!(answer["schema_version"], CONTENT_SCHEMA_VERSION);
        let data = &answer["data"];
        assert_eq!(data["vcs"], "git");
        assert!(data["reason"].is_null());
        assert_eq!(data["mode"], "working");
        assert_eq!(data["truncated"], false);
        let list = data["files"].as_array().unwrap();
        assert_eq!(list.len(), 2, "{list:?}");
        let modified = list.iter().find(|f| f["path"] == "src/a.ts").unwrap();
        assert_eq!(modified["status"], "modified");
        assert_eq!(modified["additions"], 1);
        assert_eq!(modified["deletions"], 1);
        let untracked = list.iter().find(|f| f["path"] == "notes.md").unwrap();
        assert_eq!(untracked["status"], "untracked");

        // A commit on a side branch shows up against the default branch.
        for args in [
            vec!["checkout", "-q", "-b", "feat"],
            vec!["add", "src/a.ts"],
            vec!["commit", "-q", "-m", "change"],
        ] {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}");
        }
        let Json(branch) = files("w1:p1", Some("branch")).await.expect("lists");
        assert_eq!(branch["data"]["mode"], "branch");
        assert_eq!(branch["data"]["base"], "main");
        assert!(branch["data"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "src/a.ts"));

        let refusal = files("w1:p1", Some("committed")).await.expect_err("mode");
        assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
        assert_eq!(code(&refusal), "invalid_mode");

        // Outside a checkout, with no cwd, or with a deleted cwd: the same
        // empty answer the agent route gives, not an error.
        for id in ["w1:p2", "w1:p3", "w1:p4"] {
            let Json(none) = files(id, None).await.expect("answers");
            assert!(none["data"]["vcs"].is_null(), "{id}");
            assert_eq!(none["data"]["reason"], "not_a_repository", "{id}");
            assert_eq!(none["data"]["files"], json!([]), "{id}");
        }

        let refusal = files("w1:p9", None).await.expect_err("no such pane");
        assert_eq!(refusal.0, StatusCode::NOT_FOUND);
        assert_eq!(code(&refusal), "unknown_pane");

        let refusal = Box::pin(pane_vcs_files(
            State(state.clone()),
            pane("w1:p1"),
            Query(VcsFilesQuery { mode: None }),
            bearer_headers("not-a-token"),
        ))
        .await
        .expect_err("needs a device");
        assert_eq!(refusal.0, StatusCode::FORBIDDEN);

        let file = |id: &str, query: VcsFileQuery| {
            Box::pin(pane_vcs_file(
                State(state.clone()),
                pane(id),
                Query(query),
                bearer_headers("token"),
            ))
        };
        let Json(one) = file("w1:p1", file_query(None, "notes.md", None))
            .await
            .expect("patch");
        assert_eq!(one["data"]["path"], "notes.md");
        assert_eq!(one["data"]["status"], "untracked");
        assert!(one["data"]["patch"].as_str().unwrap().contains("+new"));

        let Json(wide) = file("w1:p1", file_query(Some("branch"), "src/a.ts", Some(0)))
            .await
            .expect("patch");
        let patch = wide["data"]["patch"].as_str().unwrap();
        assert!(patch.contains("-const b = 2;\n+const B = 2;"), "{patch}");
        // Zero context: the unchanged first line is not in the patch.
        assert!(
            !patch.lines().any(|line| line == " const a = 1;"),
            "{patch}"
        );

        for unknown in ["nope.txt", "../x", "/etc/passwd", "*", "src"] {
            let refusal = file("w1:p1", file_query(None, unknown, None))
                .await
                .expect_err("not a change");
            assert_eq!(refusal.0, StatusCode::NOT_FOUND, "{unknown:?}");
            assert_eq!(code(&refusal), "unknown_path", "{unknown:?}");
        }
        let refusal = file("w1:p1", file_query(Some("staged"), "src/a.ts", None))
            .await
            .expect_err("bad mode");
        assert_eq!(code(&refusal), "invalid_mode");
        for id in ["w1:p2", "w1:p3", "w1:p4"] {
            let refusal = file(id, file_query(None, "src/a.ts", None))
                .await
                .expect_err("no checkout");
            assert_eq!(refusal.0, StatusCode::NOT_FOUND, "{id}");
            assert_eq!(code(&refusal), "not_a_repository", "{id}");
        }
        let refusal = file("w1:p9", file_query(None, "src/a.ts", None))
            .await
            .expect_err("no such pane");
        assert_eq!(code(&refusal), "unknown_pane");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn pane_vcs_discard_restores_deletes_and_refuses() {
        let (state, root, repo) = pane_state("pane-vcs-discard");
        let discard = |id: &str, path: &str, headers: HeaderMap| {
            Box::pin(discard_pane_vcs_file(
                State(state.clone()),
                pane(id),
                headers,
                Json(VcsDiscardBody {
                    path: path.to_string(),
                }),
            ))
        };

        let refusal = discard("w1:p1", "src/a.ts", bearer_headers("wrong"))
            .await
            .expect_err("needs a device");
        assert_eq!(refusal.0, StatusCode::FORBIDDEN);
        assert!(std::fs::read_to_string(repo.join("src/a.ts"))
            .unwrap()
            .contains("const B"));

        let Json(restored) = discard("w1:p1", "src/a.ts", bearer_headers("token"))
            .await
            .expect("restores");
        assert_eq!(restored["data"]["path"], "src/a.ts");
        assert_eq!(restored["data"]["action"], "restored");
        assert!(std::fs::read_to_string(repo.join("src/a.ts"))
            .unwrap()
            .contains("const b"));

        let Json(deleted) = discard("w1:p1", "notes.md", bearer_headers("token"))
            .await
            .expect("deletes");
        assert_eq!(deleted["data"]["action"], "deleted");
        assert!(!repo.join("notes.md").exists());

        // Already restored, a glob, a directory, an escape: none is a row.
        for unknown in ["nope.txt", "../x", "src/a.ts", "*", "src", "/etc/passwd"] {
            let refusal = discard("w1:p1", unknown, bearer_headers("token"))
                .await
                .expect_err("unknown");
            assert_eq!(refusal.0, StatusCode::NOT_FOUND, "{unknown:?}");
            assert_eq!(code(&refusal), "unknown_path", "{unknown:?}");
        }

        for id in ["w1:p2", "w1:p3", "w1:p4"] {
            let refusal = discard(id, "src/a.ts", bearer_headers("token"))
                .await
                .expect_err("no checkout");
            assert_eq!(refusal.0, StatusCode::NOT_FOUND, "{id}");
            assert_eq!(code(&refusal), "not_a_repository", "{id}");
        }
        let refusal = discard("w1:p9", "src/a.ts", bearer_headers("token"))
            .await
            .expect_err("no such pane");
        assert_eq!(refusal.0, StatusCode::NOT_FOUND);
        assert_eq!(code(&refusal), "unknown_pane");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn discovery_announces_the_pane_changes_routes() {
        let state = test_state("admin", Vec::new());
        let body = discovery::build_discovery(&state, false, true).await;
        assert!(body["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "pane_vcs_files"));
        assert_eq!(body["planes"]["terminal"]["features"]["vcsFiles"], true);
    }
}
