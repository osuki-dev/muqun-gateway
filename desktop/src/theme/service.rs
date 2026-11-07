//! Theme commands are accepted but not applied while the Rune UI has no
//! Blinc-token theme pipeline. Keeps the command / inbox contract compiling.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::model::commands::Command;
use crate::model::inbox::{Inbox, Update};
use crate::model::projection::ConfigStore;

static PATHS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

#[derive(Debug, Clone)]
pub enum ThemeUpdate {
    Notice(Notice),
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub message: String,
    pub detail: Option<String>,
}

pub fn set_paths(config_dir: &Path, state_dir: &Path) {
    let _ = PATHS.set((config_dir.to_path_buf(), state_dir.to_path_buf()));
}

pub fn store_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("themes")
}

pub async fn run(command: Command, inbox: Arc<Inbox>, _config: Arc<ConfigStore>) {
    let message = match command {
        Command::ThemeRefresh => "Themes UI not yet ported to Rune".into(),
        Command::ThemeInstall(id) => format!("Theme install deferred: {id}"),
        Command::ThemeRemove(id) => format!("Theme remove deferred: {id}"),
        Command::ThemeApply(id, _) => format!("Theme apply deferred: {id}"),
        Command::ThemeImportFile(path) => format!("Theme import deferred: {}", path.display()),
        Command::ThemeImportUrl(url) => format!("Theme import deferred: {url}"),
        _ => return,
    };
    inbox.push(Update::Theme(Box::new(ThemeUpdate::Notice(Notice {
        message,
        detail: None,
    }))));
}
