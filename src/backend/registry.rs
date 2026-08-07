//! Static registry for the terminal adapters shipped in this binary.
//!
//! This is intentionally not a dynamic plugin loader. It centralizes adapter
//! construction and presentation defaults while the serialized `sessions`
//! array remains backward compatible.

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context as _;

use super::{BackendKind, HerdrBackend, TerminalBackend, TmuxBackend};

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
            BackendKind::Herdr => Box::new(HerdrBackend::new(socket_path)),
            BackendKind::Tmux => Box::new(TmuxBackend::new(
                (!socket_path.is_empty()).then(|| PathBuf::from(socket_path)),
            )),
        }
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
                let output = Command::new("tmux")
                    .arg("-V")
                    .output()
                    .context("tmux backend selected, but tmux was not found on PATH")?;
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
}
