//! Launch the installed Git TUI in a new tab belonging to the source pane.

use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};

use crate::{
    api_error, backend_api_error, content_envelope, find_session, require_device, terminal_backend,
    ApiResult, AppState, BackendCreateTab, BackendPaneId, BackendSendTextMode,
};

use super::routes::{checkout_of, pane_fenced_cwd};

fn launch_command(executable: &FsPath) -> String {
    // Only a resolved executable is interpolated. The repository is passed as
    // the new tab's native cwd, never as text through the shell.
    format!("'{}'", executable.to_string_lossy().replace('\'', "'\\''"))
}

async fn repository(
    state: &AppState,
    session: &crate::SessionConfig,
    pane_id: &str,
) -> Option<PathBuf> {
    let cwd = pane_fenced_cwd(state, session, pane_id).await?;
    checkout_of(&cwd).await
}

pub(crate) async fn availability(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let installed = crate::tasks::find_on_path("lazygit").is_some();
    let root = if installed {
        repository(&state, session, &pane_id).await
    } else {
        None
    };
    Ok(Json(content_envelope(json!({
        "available": installed && root.is_some(),
        "installed": installed,
        "reason": if !installed { Some("not_installed") } else if root.is_none() { Some("not_a_repository") } else { None },
    }))))
}

pub(crate) async fn launch(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let executable = crate::tasks::find_on_path("lazygit").ok_or_else(|| {
        api_error(
            StatusCode::CONFLICT,
            "lazygit_not_installed",
            "Install lazygit on the Gateway host",
        )
    })?;
    let root = repository(&state, session, &pane_id).await.ok_or_else(|| {
        api_error(
            StatusCode::CONFLICT,
            "not_a_repository",
            "The source pane is not in an accessible Git repository",
        )
    })?;
    let backend = terminal_backend(session);
    let source = backend
        .get_pane(&BackendPaneId::new(pane_id))
        .await
        .map_err(backend_api_error)?;
    let tab = backend
        .create_tab(&BackendCreateTab {
            workspace_id: Some(source.workspace_id),
            cwd: Some(root),
            label: Some("lazygit".to_owned()),
            focus: false,
        })
        .await
        .map_err(backend_api_error)?;
    let pane = backend
        .list_panes()
        .await
        .map_err(backend_api_error)?
        .into_iter()
        .find(|pane| pane.tab_id == tab.id)
        .ok_or_else(|| {
            backend_api_error(crate::backend::BackendError::InvalidResponse(
                "created pane",
            ))
        })?;

    // Creation is acknowledged before startup. Never automatically repeat a
    // launch after any input might have reached this dedicated pane.
    let target = || {
        json!({
            "session_id": session_id,
            "workspace_id": pane.workspace_id.as_str(),
            "tab_id": tab.id.as_str(),
            "pane_id": pane.id.as_str(),
        })
    };
    let sent = backend
        .send_text(
            &pane.id,
            &launch_command(&executable),
            BackendSendTextMode::Paste,
        )
        .await;
    if sent.is_err()
        || backend
            .send_keys(&pane.id, &["Enter".to_owned()])
            .await
            .is_err()
    {
        return Ok(Json(content_envelope(
            json!({ "target": target(), "started": false, "reason": "startup_failed" }),
        )));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if backend
            .get_pane(&pane.id)
            .await
            .ok()
            .is_some_and(|current| {
                current
                    .foreground_command
                    .as_deref()
                    .is_some_and(super::shortcuts::is_lazygit_command)
            })
        {
            return Ok(Json(content_envelope(
                json!({ "target": target(), "started": true }),
            )));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Json(content_envelope(
                json!({ "target": target(), "started": false, "reason": "startup_unconfirmed" }),
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{bearer_headers, test_device, test_state};

    #[tokio::test]
    async fn both_operations_require_a_paired_device_before_inspecting_the_host() {
        for launch_operation in [false, true] {
            let state = test_state("admin", Vec::new());
            let path = Path(("default".to_owned(), "missing".to_owned()));
            let result = if launch_operation {
                launch(State(state), path, HeaderMap::new()).await
            } else {
                availability(State(state), path, HeaderMap::new()).await
            };
            assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn unknown_sessions_are_refused_without_starting_a_program() {
        let state = test_state("admin", vec![test_device("qa", "token")]);
        let result = launch(
            State(state),
            Path(("missing".to_owned(), "pane".to_owned())),
            bearer_headers("token"),
        )
        .await;
        assert_eq!(result.unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn foreground_detection_is_specific_and_does_not_match_agent_titles() {
        assert!(super::super::shortcuts::is_lazygit_command(
            "/usr/bin/lazygit"
        ));
        assert!(!super::super::shortcuts::is_lazygit_command(
            "claude lazygit"
        ));
        assert!(!super::super::shortcuts::is_lazygit_command(
            "lazygit-other"
        ));
        let value = crate::shortcuts::resolve(None, Some("lazygit"), None);
        assert_eq!(value["profile"], "lazygit");
        assert!(value["keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| key["key"] == "q"));
        assert!(!value["keyActions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| key["key"].as_str().unwrap_or("").starts_with("nvim:")));
    }

    #[test]
    fn executable_paths_remain_one_shell_word() {
        assert_eq!(
            launch_command(FsPath::new("/usr/bin/lazygit")),
            "'/usr/bin/lazygit'"
        );
        assert_eq!(
            launch_command(FsPath::new("/tmp/user's $(touch nope)/lazygit")),
            "'/tmp/user'\\''s $(touch nope)/lazygit'"
        );
    }
    #[tokio::test]
    #[ignore = "requires installed tmux and lazygit"]
    async fn real_git_tui_uses_a_dedicated_tab_and_preserves_source_focus() {
        use crate::test_support::BackendKind;
        use crate::{BackendCreateWorkspace, SessionConfig};
        use std::process::Command;
        struct Sandbox(PathBuf, PathBuf);
        impl Drop for Sandbox {
            fn drop(&mut self) {
                let _ = Command::new("tmux")
                    .arg("-S")
                    .arg(&self.0)
                    .arg("kill-server")
                    .output();
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let socket = PathBuf::from(format!("/tmp/gw-lazygit-{}.sock", &id[..12]));
        let root = std::env::temp_dir().join(format!("gw-lazygit-{id}"));
        std::fs::create_dir_all(&root).unwrap();
        let _sandbox = Sandbox(socket.clone(), root.clone());
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        let mut state = test_state("admin", vec![test_device("qa", "token")]);
        state.config.sessions = vec![SessionConfig {
            id: "default".into(),
            label: "QA".into(),
            socket_path: socket.to_string_lossy().into_owned(),
            backend: BackendKind::Tmux,
        }];
        let backend = terminal_backend(&state.config.sessions[0]);
        backend
            .create_workspace(&BackendCreateWorkspace {
                cwd: Some(root),
                label: Some("lazygit-qa".into()),
                focus: true,
            })
            .await
            .unwrap();
        let source = backend.list_panes().await.unwrap().remove(0);
        let path = || Path(("default".to_owned(), source.id.as_str().to_owned()));
        let before = backend
            .list_tabs()
            .await
            .unwrap()
            .into_iter()
            .find(|tab| tab.focused)
            .unwrap()
            .id;
        let available = availability(State(state.clone()), path(), bearer_headers("token"))
            .await
            .unwrap()
            .0;
        assert_eq!(available["data"]["available"], true);
        let result = launch(State(state), path(), bearer_headers("token"))
            .await
            .unwrap()
            .0;
        assert_eq!(result["data"]["started"], true, "{result}");
        let target = BackendPaneId::new(result["data"]["target"]["pane_id"].as_str().unwrap());
        assert_ne!(target, source.id);
        assert_eq!(backend.list_panes().await.unwrap().len(), 2);
        assert_eq!(
            backend
                .list_tabs()
                .await
                .unwrap()
                .into_iter()
                .find(|tab| tab.focused)
                .unwrap()
                .id,
            before
        );
        backend.send_keys(&target, &["q".into()]).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let pane = backend.get_pane(&target).await.unwrap();
            if !pane
                .foreground_command
                .as_deref()
                .is_some_and(super::super::shortcuts::is_lazygit_command)
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Quit key did not reach lazygit"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
