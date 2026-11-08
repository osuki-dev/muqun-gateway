//! Where Muqun Desktop keeps its files, and how it shares the machine with a
//! gateway service.

pub mod coexist;
pub mod dialogs;
pub mod editors;
pub mod filesearch;
pub mod fonts;
pub mod instance;
pub mod keys;
pub mod media;
pub mod mic;
pub mod notify;
pub mod pty;
pub mod secrets;
pub mod shell;
pub mod shutdown;
pub mod tray;

use std::path::PathBuf;

/// The product's directory name. Deliberately not the gateway's: the gateway
/// owns `muqun-gateway/` through its state lock, and a second owner there
/// would push it off the phones it is serving (architecture §4).
pub const DIR_NAME: &str = "muqun-desktop";

/// The configuration and state directories, platform-standard:
/// `~/.config/muqun-desktop` and `~/.local/share/muqun-desktop` on Linux,
/// `~/Library/Application Support/muqun-desktop` (both) on macOS, and
/// `%APPDATA%` / `%LOCALAPPDATA%` on Windows.
pub fn directories() -> anyhow::Result<(PathBuf, PathBuf)> {
    let config = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("this system names no configuration directory"))?
        .join(DIR_NAME);
    let state = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow::anyhow!("this system names no data directory"))?
        .join(DIR_NAME);
    Ok((config, state))
}
