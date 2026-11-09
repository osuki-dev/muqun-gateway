//! File handoff uses the same explicit asset policy as a local session file.
//! Only the source computer differs; the App reads with that computer's pairing.
use crate::{ApiResult, AppState};
use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

pub(crate) const CAPABILITY: &str = "mcp_file_present_v1";

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/mcp/files/present", post(present_route))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    path: String,
}
async fn present_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Request>,
) -> ApiResult<Json<Value>> {
    let caller = crate::platform::http::require_device_or_peer(&state, &headers)?;
    if caller.kind == crate::connectivity::authority::DeviceKind::Gateway
        && !state.peers.accepts_delegations()
    {
        return Err(crate::platform::http::peer_forbidden());
    }
    present(&state, body.path).await.map(Json)
}

pub(crate) async fn present(state: &AppState, path: String) -> ApiResult<Value> {
    let root = dirs::home_dir().ok_or_else(|| {
        crate::api_error(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "home_unavailable",
            "Home directory unavailable",
        )
    })?;
    let asset = crate::agents::message_images::explicit_asset(state, path, root, None).await?;
    Ok(
        json!({"state":"done", "server_id":state.config.server_id, "machine":state.config.label, "files":[asset]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    #[tokio::test]
    async fn file_handoff_requires_a_paired_caller_before_reading() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let state = crate::test_state("admin", vec![crate::test_device("phone", "paired")]);
        let app = mount(Router::new()).with_state(state);
        for token in [None, Some("wrong"), Some("paired")] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/mcp/files/present")
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(
                    request
                        .body(Body::from(r#"{"path":"/etc/passwd"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            if token == Some("paired") {
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
            } else if token.is_none() {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            } else {
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
            }
        }
    }
    #[tokio::test]
    async fn disabling_remote_tasks_also_refuses_remote_file_handoffs() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let state = crate::test_state(
            "admin",
            vec![crate::test_support::test_peer_device("peer", "peer-token")],
        );
        state.peers.set_accepts_delegations(false).unwrap();
        let response = mount(Router::new())
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp/files/present")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer peer-token")
                    .body(Body::from(r#"{"path":"/tmp/report.txt"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}

/// Output references are UI hints, never grants: nothing is read until the
/// paired user opens one through the ordinary explicit-file policy. When the
/// output was produced on this computer (`local`), a candidate is kept only if
/// it names an existing regular file here; another computer's paths are not
/// statted here, where they would mean something else.
pub(crate) fn attach_output(answer: &mut Value, server_id: &str, machine: &str, local: bool) {
    if !answer.is_object() || server_id.is_empty() {
        return;
    }
    let mut paths = Vec::<String>::new();
    let mut add = |raw: &str| {
        let path = raw.trim().trim_matches('`');
        if paths.len() == 8
            || path.len() > 4096
            || path.chars().any(char::is_control)
            || !(path.starts_with('/') || path.starts_with("~/"))
            || path.starts_with("//")
            || path.ends_with('/')
            || ["/dev", "/proc", "/sys"].iter().any(|root| {
                path.strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            || paths.iter().any(|held| held == path)
        {
            return;
        }
        if local
            && !crate::agents::directories::expand_home(path)
                .and_then(|full| std::fs::metadata(full).ok())
                .is_some_and(|meta| meta.is_file())
        {
            return;
        }
        paths.push(path.to_owned());
    };
    fn scan(add: &mut impl FnMut(&str), value: &Value, depth: usize) {
        if depth > 4 {
            return;
        }
        match value {
            Value::Object(fields) => {
                for (key, value) in fields.iter().take(64) {
                    if matches!(
                        key.as_str(),
                        "path" | "file" | "file_path" | "filepath" | "uri"
                    ) {
                        if let Some(path) = value.as_str() {
                            add(path);
                        }
                    } else if value.is_object() || value.is_array() {
                        scan(add, value, depth + 1);
                    }
                }
            }
            Value::Array(items) => {
                for item in items.iter().take(64) {
                    scan(add, item, depth + 1);
                }
            }
            _ => {}
        }
    }
    for key in ["stdout_tail", "answer", "summary"] {
        let Some(text) = answer[key].as_str() else {
            continue;
        };
        if text.len() > 64 * 1024 {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            scan(&mut add, &value, 0);
        }
        for line in text.lines().take(512) {
            let line = line.trim().strip_prefix("- ").unwrap_or(line.trim());
            if let Some(path) = bare_path(line) {
                add(path);
            }
            // Markdown file links are explicit file references, independent of extension.
            for link in line.split("](").skip(1) {
                if let Some((path, _)) = link.split_once(')') {
                    add(path.trim_matches(['<', '>']));
                }
            }
        }
    }
    answer["server_id"] = json!(server_id);
    answer["machine"] = json!(machine);
    if !paths.is_empty() {
        answer["files"] = json!(paths.into_iter().map(|path| {
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            json!({"id":path,"path":path,"name":name,"mime":"application/octet-stream","kind":"binary","size":0,"previewable":false})
        }).collect::<Vec<_>>());
    }
}

/// A plain output line that is one path and nothing else: `ls -1` or `find`
/// output, an agent printing what it wrote. Table rows that merely start with
/// a path (`df`, `mount`, `lsof`) and `path: message` lines are not. A path
/// with spaces counts only when the whole line quotes it.
fn bare_path(line: &str) -> Option<&str> {
    let line = line.trim_matches('`');
    let quoted = ['"', '\''].iter().find_map(|&quote| {
        line.strip_prefix(quote)?
            .strip_suffix(quote)
            .filter(|inner| !inner.contains(quote))
    });
    let path = quoted.unwrap_or(line);
    let spaced = match quoted {
        Some(inner) => inner.contains('\t') || inner.contains("  "),
        None => line.contains(char::is_whitespace),
    };
    ((path.starts_with('/') || path.starts_with("~/")) && !spaced).then_some(path)
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn ordinary_output_paths_are_bound_to_the_executing_computer() {
        let mut answer = json!({"stdout_tail": "/tmp/request.certSigningRequest\n[Download](</tmp/a file.unknown>)\n/tmp/request.certSigningRequest\n/tmp/program: replacing existing signature", "server_id":"untrusted"});
        attach_output(&mut answer, "source-id", "mini", false);
        assert_eq!(answer["server_id"], "source-id");
        assert_eq!(answer["files"].as_array().unwrap().len(), 2);
        assert_eq!(
            answer["files"][0]["path"],
            "/tmp/request.certSigningRequest"
        );
        assert_eq!(answer["files"][1]["path"], "/tmp/a file.unknown");
        assert!(answer["files"][0].get("download_url").is_none());
    }

    #[test]
    fn structured_results_do_not_accept_remote_urls_or_unbounded_file_lists() {
        let mut answer = json!({"stdout_tail": json!({"items":[{"path":"https://other/a"},{"file":"//other/a"},{"file_path":"~/Desktop/report"}]}).to_string()});
        attach_output(&mut answer, "mini-id", "mini", false);
        assert_eq!(answer["files"].as_array().unwrap().len(), 1);
        assert_eq!(answer["files"][0]["path"], "~/Desktop/report");
        let mut answer = json!({"stdout_tail": (0..20).map(|n| format!("/tmp/{n}")).collect::<Vec<_>>().join("\n")});
        attach_output(&mut answer, "mini-id", "mini", false);
        assert_eq!(answer["files"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn only_lines_that_are_one_whole_path_become_files() {
        let cases: &[(&str, &str, &[&str])] = &[
            (
                "df -h on macOS",
                "Filesystem        Size    Used   Avail Capacity iused ifree %iused  Mounted on\n\
                 /dev/disk3s1s1   460Gi    10Gi    42Gi    20%    426k  444M    0%   /\n\
                 /dev/disk3s5     460Gi    47Gi    42Gi    53%      47  444M    0%   /System/Volumes/VM\n\
                 map auto_home      0Bi     0Bi     0Bi   100%       0     0     -   /System/Volumes/Data/home",
                &[],
            ),
            (
                "df -h on Linux",
                "Filesystem      Size  Used Avail Use% Mounted on\n\
                 /dev/nvme0n1p2  930G  412G  517G  45% /\n\
                 tmpfs            32G  1.2M   32G   1% /tmp\n\
                 /dev/nvme0n1p1 1022M  196M  827M  20% /boot",
                &[],
            ),
            (
                "mount",
                "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                 proc on /proc type proc (rw,nosuid,nodev,noexec,relatime)",
                &[],
            ),
            ("device nodes", "/dev/null\n/proc/self/status\n/sys/kernel/mm", &[]),
            (
                "ls -1 and find",
                "/var/log/app.log\n/var/log/old/\n./relative.log\n/srv/x/report.pdf",
                &["/var/log/app.log", "/srv/x/report.pdf"],
            ),
            (
                "agent summary",
                "Wrote the report:\n- `/tmp/out/report.html`\n\"/tmp/my notes.txt\"\n/tmp/two  spaces",
                &["/tmp/out/report.html", "/tmp/my notes.txt"],
            ),
            (
                "path: message and unquoted spaces",
                "/tmp/program: replacing existing signature\n/tmp/a file.txt\n/usr/bin/ld: cannot find -lfoo",
                &[],
            ),
            (
                "markdown links",
                "See [the log](</tmp/build output.log>) and [x](/dev/null)",
                &["/tmp/build output.log"],
            ),
            (
                "JSON result",
                r#"{"items":[{"path":"/tmp/a.json"},{"path":"/dev/stdout"},{"name":"/tmp/not-a-path-key"}]}"#,
                &["/tmp/a.json"],
            ),
        ];
        for (name, output, want) in cases {
            let mut answer = json!({ "stdout_tail": output });
            attach_output(&mut answer, "mini-id", "mini", false);
            let got: Vec<&str> = answer["files"]
                .as_array()
                .map(|files| files.iter().map(|f| f["path"].as_str().unwrap()).collect())
                .unwrap_or_default();
            assert_eq!(&got, want, "{name}");
        }
    }

    #[test]
    fn local_output_keeps_only_existing_regular_files() {
        let dir = crate::test_support::asset_test_dir("mcp-output-local");
        let file = dir.join("made.txt");
        std::fs::write(&file, b"x").unwrap();
        let output = format!(
            "{}\n{}\n{}",
            file.display(),
            dir.display(),
            dir.join("gone.txt").display()
        );
        let mut answer = json!({ "stdout_tail": output });
        attach_output(&mut answer, "self-id", "here", true);
        let files = answer["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], file.to_string_lossy().as_ref());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn unknown_extensions_use_the_existing_asset_download_service() {
        let dir = crate::test_support::asset_test_dir("mcp-explicit-file");
        let path = dir.join("sample.unrecognized");
        std::fs::write(&path, [0u8, 255, 0, 255]).unwrap();
        let state = crate::test_state("admin", vec![]);
        let result = present(&state, path.to_string_lossy().into_owned())
            .await
            .unwrap();
        let file = &result["files"][0];
        assert_eq!(file["kind"], "binary");
        assert_eq!(file["size"], 4);
        assert!(file["download_url"]
            .as_str()
            .unwrap()
            .starts_with("/api/agent-images/"));
        assert!(file.get("upload_name").is_none());
        assert_eq!(
            file["path"],
            path.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
