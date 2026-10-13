//! Discover an existing pinned Codex daemon. Never creates paths or starts a daemon.
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use tokio::sync::mpsc;

use super::{CodexClient, CodexEvent};
use crate::agents::ports::agent::AgentError;

type Connection = (Arc<CodexClient>, mpsc::Receiver<CodexEvent>);

#[cfg(not(test))]
pub(crate) async fn connect_existing() -> Result<Option<Connection>, AgentError> {
    #[cfg(unix)]
    {
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|p| p.join(".codex")))
            .ok_or_else(|| unavailable("Codex home unavailable"))?;
        connect_at(&home).await
    }
    #[cfg(not(unix))]
    Ok(None)
}

#[cfg(test)]
pub(crate) async fn connect_existing() -> Result<Option<Connection>, AgentError> {
    // Unit tests must never discover or attach the user's daemon.
    Ok(None)
}

#[cfg(unix)]
fn unavailable(message: &str) -> AgentError {
    AgentError::NotAvailable(message.into())
}

#[cfg(unix)]
fn verified_socket(home: &Path) -> Result<Option<(PathBuf, PathBuf)>, AgentError> {
    let directory = std::fs::canonicalize("/tmp")
        .map_err(|_| unavailable("Codex daemon socket root unavailable"))?
        .join(format!("codex-daemon-{}", unsafe { libc::geteuid() }));
    verified_socket_in(home, &directory)
}

#[cfg(unix)]
fn verified_socket_in(
    home: &Path,
    directory: &Path,
) -> Result<Option<(PathBuf, PathBuf)>, AgentError> {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    if !home.is_absolute() {
        return Err(unavailable("Codex home must be absolute"));
    }
    let alias = home.join("app-server-control/app-server-control.sock");
    let alias_metadata = match std::fs::symlink_metadata(&alias) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unavailable("Codex daemon socket cannot be inspected")),
    };
    let uid = unsafe { libc::geteuid() };
    let check = || -> std::io::Result<(PathBuf, PathBuf)> {
        let home = std::fs::canonicalize(home)?;
        let parent = std::fs::canonicalize(alias.parent().expect("fixed child path"))?;
        let parent_meta = std::fs::metadata(&parent)?;
        let safe_parent = parent_meta.is_dir()
            && (parent_meta.uid() == 0 || parent_meta.uid() == uid)
            && (parent_meta.mode() & 0o022 == 0 || parent_meta.mode() & 0o1000 != 0);
        let directory_meta = std::fs::symlink_metadata(directory)?;
        let hash = Sha256::digest(
            parent
                .join("app-server-control.sock")
                .as_os_str()
                .as_bytes(),
        );
        let physical = directory.join(hex::encode(hash));
        let metadata = std::fs::symlink_metadata(&physical)?;
        if !safe_parent
            || !alias_metadata.file_type().is_symlink()
            || std::fs::read_link(&alias)? != physical
            || !directory_meta.is_dir()
            || directory_meta.uid() != uid
            || directory_meta.mode() & 0o777 != 0o700
            || !metadata.file_type().is_socket()
            || metadata.uid() != uid
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        Ok((home, physical))
    };
    check()
        .map(Some)
        .map_err(|_| unavailable("Codex daemon socket ownership or protection check failed"))
}

#[cfg(unix)]
pub(crate) async fn connect_at(home: &Path) -> Result<Option<Connection>, AgentError> {
    let Some((home, socket)) = verified_socket(home)? else {
        return Ok(None);
    };
    let connect = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|_| unavailable("Codex daemon connection unavailable"))?;
        if stream.peer_cred().map(|peer| peer.uid()).ok() != Some(unsafe { libc::geteuid() }) {
            return Err(unavailable("Codex daemon peer identity mismatch"));
        }
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_frame_size(Some(super::client::MAX_FRAME_BYTES))
            .max_message_size(Some(super::client::MAX_FRAME_BYTES));
        let (socket, _) =
            tokio_tungstenite::client_async_with_config("ws://localhost/rpc", stream, Some(config))
                .await
                .map_err(|_| unavailable("Codex daemon handshake failed"))?;
        Ok(CodexClient::from_websocket(socket, home))
    };
    let (client, events) = tokio::time::timeout(Duration::from_secs(3), connect)
        .await
        .map_err(|_| unavailable("Codex daemon connection timed out"))??;
    if let Err(error) = client.initialize().await {
        client.stop();
        return Err(error);
    }
    Ok(Some((client, events)))
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::net::UnixListener;

    pub(crate) struct TestDirectory(pub PathBuf);
    impl TestDirectory {
        pub(crate) fn new() -> Self {
            // Leave room for the official 64-byte hash within macOS SUN_LEN.
            let path = PathBuf::from("/tmp").join(format!(
                "mqcd-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn daemon_socket_requires_official_alias_and_private_protected_socket() {
        let root = TestDirectory::new();
        let home = root.0.join("home");
        let parent = home.join("app-server-control");
        let protected = root.0.join("p");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::create_dir(&protected).unwrap();
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = parent.join("app-server-control.sock");
        assert!(verified_socket_in(&home, &protected).unwrap().is_none());
        let physical = protected.join(hex::encode(Sha256::digest(alias.as_os_str().as_bytes())));
        let listener = UnixListener::bind(&physical).unwrap();
        std::fs::set_permissions(&physical, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&physical, &alias).unwrap();
        assert_eq!(
            verified_socket_in(&home, &protected).unwrap(),
            Some((home.clone(), physical.clone()))
        );
        std::fs::set_permissions(&physical, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(verified_socket_in(&home, &protected).is_err());
        std::fs::set_permissions(&physical, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verified_socket_in(&home, &protected).is_err());
        std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(root.0.join("other.sock"), &alias).unwrap();
        assert!(verified_socket_in(&home, &protected).is_err());
        std::fs::remove_file(&alias).unwrap();
        symlink(&physical, &alias).unwrap();
        drop(listener);
        std::fs::remove_file(&physical).unwrap();
        std::fs::write(&physical, "not a socket").unwrap();
        assert!(verified_socket_in(&home, &protected).is_err());
    }

    #[tokio::test]
    async fn daemon_tests_never_discover_the_user_socket() {
        assert!(connect_existing().await.unwrap().is_none());
        let directory = TestDirectory::new();
        assert!(connect_at(&directory.0).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "explicit read-only connection to an existing local daemon"]
    async fn codex_shared_daemon_read_only_probe() {
        let home = std::env::var_os("MUQUN_CODEX_DAEMON_PROBE_HOME")
            .map(PathBuf::from)
            .expect("explicit probe home required");
        let (client, _events) = connect_at(&home).await.unwrap().expect("existing daemon");
        let page = client
            .call("thread/loaded/list", serde_json::json!({"limit": 100}))
            .await
            .unwrap();
        let count = page["data"].as_array().unwrap().len();
        assert!(count <= 100);
        assert!(client.is_shared());
        println!("verified_shared_daemon=true loaded_page_count={count}");
        client.stop();
    }
}
