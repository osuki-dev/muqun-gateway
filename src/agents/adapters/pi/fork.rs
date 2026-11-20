//! Whole-session cloning without letting Pi initialize or rewrite the source.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::rpc::{PiChild, PiCommandLine, PiHost, PiLaunch};
use super::sessions::{SessionFile, SessionStore};
use crate::agents::domain::{AgentSessionInfo, AgentSessionStatus, SessionForkInfo};
use crate::agents::ports::AgentError;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // This random, owner-only directory was created by this operation.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) async fn clone_session(
    cmd: &PiCommandLine,
    store: &SessionStore,
    host: &Arc<PiHost>,
    stop: &CancellationToken,
    session_id: &str,
) -> Result<AgentSessionInfo, AgentError> {
    let source = store.find(session_id).await?;
    let original = super::sessions::open_regular(&source.path)
        .await
        .map_err(|_| failed())?
        .into_std()
        .await;
    let parent = cmd.extension.parent().ok_or_else(failed)?;
    if std::fs::symlink_metadata(parent)
        .map_err(|_| failed())?
        .file_type()
        .is_symlink()
    {
        return Err(failed());
    }
    let scratch = parent.join(format!(".clone-{}", uuid::Uuid::new_v4()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&scratch).map_err(|_| failed())?;
    let scratch = Scratch(scratch);
    let output = scratch.0.join("output");
    builder.create(&output).map_err(|_| failed())?;
    let snapshot = scratch.0.join(source.path.file_name().ok_or_else(failed)?);
    let snapshot = tokio::task::spawn_blocking(move || {
        let size = original.metadata()?.len();
        let mut copy = create(&snapshot)?;
        let n = std::io::copy(&mut std::io::Read::take(original, size), &mut copy)?;
        if n != size {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        Ok(snapshot)
    })
    .await
    .map_err(|_| failed())?
    .map_err(|_| failed())?;
    if stop.is_cancelled() {
        return Err(failed());
    }
    let child = Arc::new(
        PiChild::spawn(
            cmd,
            &PiLaunch {
                session_file: Some(snapshot),
                session_id: None,
                cwd: PathBuf::from(&source.cwd),
                env: vec![(
                    "PI_CODING_AGENT_SESSION_DIR".into(),
                    output.to_string_lossy().into_owned(),
                )],
                no_extensions: true,
            },
        )
        .await?,
    );
    let mut owned = host.hold_probe(child.clone());
    let operation = async {
        let result = child.request(json!({"type":"clone"})).await?;
        if result.get("cancelled").and_then(Value::as_bool) != Some(false) {
            return Err(failed());
        }
        let state = child.request(json!({"type":"get_state"})).await?;
        let id = state
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| *id != session_id)
            .ok_or_else(failed)?;
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(failed());
        }
        let path = PathBuf::from(
            state
                .get("sessionFile")
                .and_then(Value::as_str)
                .ok_or_else(failed)?,
        );
        let path = tokio::fs::canonicalize(path).await.map_err(|_| failed())?;
        if !path.starts_with(
            tokio::fs::canonicalize(&output)
                .await
                .map_err(|_| failed())?,
        ) || path.extension().and_then(|s| s.to_str()) != Some("jsonl")
        {
            return Err(failed());
        }
        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.split_once('_'))
            .map(|(_, id)| id);
        if filename != Some(id) {
            return Err(failed());
        }
        let file = SessionFile {
            id: id.to_string(),
            path,
            cwd: source.cwd.clone(),
            created_ms: 0,
            modified_ms: 0,
        };
        let contents = super::sessions::open_regular(&file.path)
            .await
            .map_err(|_| failed())?
            .into_std()
            .await;
        Ok((file, contents))
    };
    let result = tokio::select! {
        biased;
        _ = stop.cancelled() => Err(failed()),
        result = operation => result,
    };
    child.close().await;
    owned.finish();
    let (file, contents) = result?;
    if stop.is_cancelled() {
        return Err(failed());
    }
    let root = tokio::fs::canonicalize(store.root())
        .await
        .map_err(|_| failed())?;
    let destination = tokio::fs::canonicalize(source.path.parent().ok_or_else(failed)?)
        .await
        .map_err(|_| failed())?;
    if !destination.starts_with(root) {
        return Err(failed());
    }
    let destination = destination.join(file.path.file_name().ok_or_else(failed)?);
    let target = destination.clone();
    let clone_id = file.id.clone();
    tokio::task::spawn_blocking(move || publish(&target, contents, &clone_id, &source.path))
        .await
        .map_err(|_| failed())?
        .map_err(|_| failed())?;
    let file = store.find(&file.id).await?;
    let mut info = store.read_summary(&file, AgentSessionStatus::Idle).await?;
    info.fork = Some(SessionForkInfo {
        session_id: session_id.to_string(),
        boundary_type: Some("session".into()),
        message_id: None,
    });
    Ok(info)
}

fn failed() -> AgentError {
    AgentError::RequestFailed("Pi session clone could not be completed; not retried".into())
}

fn create(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn publish(
    destination: &Path,
    contents: std::fs::File,
    id: &str,
    parent: &Path,
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut input = BufReader::new(contents);
    let mut line = Vec::new();
    (&mut input)
        .take(64 * 1024 + 1)
        .read_until(b'\n', &mut line)?;
    if line.len() > 64 * 1024 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    let mut header: Value = serde_json::from_slice(&line)?;
    if header["type"] != "session" || header["id"] != id {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    header["parentSession"] = json!(parent);
    let temporary =
        destination.with_file_name(format!(".muqun-clone-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut out = create(&temporary)?;
        serde_json::to_writer(&mut out, &header)?;
        out.write_all(b"\n")?;
        std::io::copy(&mut input, &mut out)?;
        out.sync_all()?;
        // Same-directory hard linking publishes a complete file atomically
        // and refuses an existing target instead of replacing user data.
        std::fs::hard_link(&temporary, destination)
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_never_replaces_an_existing_session() {
        let root = std::env::temp_dir().join(format!("muqun-pi-publish-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let destination = root.join("existing.jsonl");
        std::fs::write(&destination, "preserve existing session\n").unwrap();
        let raw = root.join("clone");
        std::fs::write(&raw, b"{\"type\":\"session\",\"id\":\"clone\"}\n").unwrap();
        assert!(publish(
            &destination,
            std::fs::File::open(&raw).unwrap(),
            "clone",
            &raw
        )
        .is_err());
        assert_eq!(
            std::fs::read_to_string(destination).unwrap(),
            "preserve existing session\n"
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}
