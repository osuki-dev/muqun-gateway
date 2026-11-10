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
pub mod pairing;
pub mod pty;
pub mod secrets;
pub mod shell;
pub mod shutdown;
pub mod tray;
pub mod update;

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

/// Whether the window is made see-through, so the sidebar and the main card
/// float on the desktop and the frame around them shows what is behind the
/// window. Linux compositors draw that, Hyprland and KWin blurred, which
/// costs the app nothing; macOS and Windows keep the opaque frame until it
/// is checked there. Under reduce transparency the app paints the frame
/// solid anyway.
pub fn see_through_window() -> bool {
    cfg!(target_os = "linux")
}

/// Whether this build is running on macOS.
pub fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

/// What the window's own controls take at the titlebar's leading edge:
/// macOS's traffic lights, as Rune reports them. Elsewhere nothing sits
/// there (a headless render still reports the Mac's, so ignore it).
pub fn titlebar_inset(reported: f32) -> f32 {
    if is_macos() {
        reported
    } else {
        0.0
    }
}
