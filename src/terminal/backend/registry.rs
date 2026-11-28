//! Static registry for the terminal adapters shipped in this binary.
//!
//! This is intentionally not a dynamic plugin loader. It centralizes adapter
//! construction and presentation defaults while the serialized `sessions`
//! array remains backward compatible.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;

use super::{BackendKind, HerdrBackend, MuqunTermBackend, TerminalBackend, TmuxBackend};

pub struct BackendRegistry {
    herdr_default_socket: String,
}

impl BackendRegistry {
    pub fn new(herdr_default_socket: impl Into<String>) -> Self {
        Self {
            herdr_default_socket: herdr_default_socket.into(),
        }
    }

    pub fn connect(&self, kind: BackendKind, socket_path: &str) -> Box<dyn TerminalBackend> {
        match kind {
            BackendKind::Herdr => {
                let muqun_socket = super::muqun_term::default_socket_path();
                if let Some(target) =
                    self.resolve_muqun_term_socket(socket_path, &muqun_socket, !cfg!(test))
                {
                    Box::new(MuqunTermBackend::new(target))
                } else {
                    Box::new(HerdrBackend::new(socket_path))
                }
            }
            BackendKind::Tmux => Box::new(TmuxBackend::new(
                (!socket_path.is_empty()).then(|| PathBuf::from(socket_path)),
            )),
        }
    }

    fn resolve_muqun_term_socket(
        &self,
        socket_path: &str,
        muqun_socket: &Path,
        allow_auto_fallback: bool,
    ) -> Option<PathBuf> {
        if socket_path.ends_with("term.sock") {
            return Some(PathBuf::from(socket_path));
        }
        if allow_auto_fallback
            && socket_path == self.herdr_default_socket
            && !Path::new(socket_path).exists()
            && muqun_socket.exists()
        {
            return Some(muqun_socket.to_path_buf());
        }
        None
    }

    pub fn default_socket(&self, kind: BackendKind) -> String {
        match kind {
            BackendKind::Herdr => self.herdr_default_socket.clone(),
            BackendKind::Tmux => String::new(),
        }
    }

    pub fn default_label(&self, kind: BackendKind) -> &'static str {
        match kind {
            BackendKind::Herdr => "Herdr",
            BackendKind::Tmux => "tmux",
        }
    }

    pub fn endpoint<'a>(&self, kind: BackendKind, socket_path: &'a str) -> Cow<'a, str> {
        match kind {
            BackendKind::Tmux if socket_path.is_empty() => Cow::Borrowed("default tmux server"),
            _ => Cow::Borrowed(socket_path),
        }
    }

    pub fn ensure_available(&self, kind: BackendKind) -> anyhow::Result<()> {
        match kind {
            BackendKind::Herdr => Ok(()),
            BackendKind::Tmux => {
                // Resolved rather than spawned by name, so that a failure here
                // can say which PATH was searched. This check runs in the
                // installer's shell and the gateway may later run under an init
                // system with a narrower one; naming the PATH is what makes the
                // two environments distinguishable to whoever reads the error.
                let path = std::env::var("PATH").unwrap_or_default();
                let program =
                    crate::login_env::lookup(super::TMUX_PROGRAM, &path).with_context(|| {
                        format!("tmux backend selected, but no `tmux` was found on PATH={path}")
                    })?;
                let output = Command::new(&program).arg("-V").output().with_context(|| {
                    format!(
                        "tmux backend selected, but {} would not run",
                        program.display()
                    )
                })?;
                anyhow::ensure!(
                    output.status.success(),
                    "tmux backend selected, but `tmux -V` failed"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_defaults_live_with_the_registered_adapter() {
        let registry = BackendRegistry::new("/run/herdr.sock");
        assert_eq!(registry.default_label(BackendKind::Herdr), "Herdr");
        assert_eq!(
            registry.default_socket(BackendKind::Herdr),
            "/run/herdr.sock"
        );
        assert_eq!(
            registry.endpoint(BackendKind::Tmux, ""),
            "default tmux server"
        );
    }

    #[test]
    fn resolves_muqun_term_when_explicit_or_when_default_herdr_absent() {
        let temp_dir =
            std::env::temp_dir().join(format!("muqun-registry-fallback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let existing_muqun_sock = temp_dir.join("term.sock");
        std::fs::write(&existing_muqun_sock, b"").unwrap();
        let missing_herdr_sock = temp_dir.join("herdr.sock");

        let registry = BackendRegistry::new(missing_herdr_sock.to_string_lossy());

        // Explicit term.sock always routes to MuqunTermBackend
        assert_eq!(
            registry.resolve_muqun_term_socket(
                "/custom/muqun-desktop/term.sock",
                &existing_muqun_sock,
                false,
            ),
            Some(PathBuf::from("/custom/muqun-desktop/term.sock"))
        );

        // Automatic fallback activates when Herdr default socket is absent and term.sock exists
        assert_eq!(
            registry.resolve_muqun_term_socket(
                &missing_herdr_sock.to_string_lossy(),
                &existing_muqun_sock,
                true,
            ),
            Some(existing_muqun_sock.clone())
        );

        // Non-default Herdr socket paths do not fall back automatically
        assert_eq!(
            registry.resolve_muqun_term_socket(
                "/some/other/herdr.sock",
                &existing_muqun_sock,
                true,
            ),
            None
        );

        // When Herdr socket exists on disk, Herdr takes precedence
        std::fs::write(&missing_herdr_sock, b"").unwrap();
        assert_eq!(
            registry.resolve_muqun_term_socket(
                &missing_herdr_sock.to_string_lossy(),
                &existing_muqun_sock,
                true,
            ),
            None
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
