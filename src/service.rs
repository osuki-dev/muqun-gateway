//! Keeping the gateway running across a logout or a reboot.
//!
//! `start` spawns a detached child and writes a pid file. That survives the
//! terminal it was launched from -- which is all it was ever asked to do -- but
//! it does not survive the machine restarting. Nothing in this project used to
//! register the gateway with an init system, so after a reboot the phone simply
//! could not reach the computer, with no signal on either side saying why, until
//! somebody came back to the machine and ran `start` again by hand.
//!
//! This module registers it, and the two rules it follows are the whole design:
//!
//! 1. **As the user, never as root.** A LaunchAgent in the user's own
//!    `~/Library/LaunchAgents`, or a systemd *user* unit in
//!    `~/.config/systemd/user`. No administrator password, nothing written
//!    outside `$HOME`, nothing that runs as another identity. That is not
//!    timidity about privileges: the gateway's whole job is to drive *this
//!    user's* tmux server, and a root daemon cannot see that socket at all.
//! 2. **Never behind the user's back.** The installer asks, and it says what
//!    the service is for before it asks. `service uninstall` reverses it
//!    completely and leaves the pairing alone.
//!
//! `linger` is the one piece with no macOS counterpart. A systemd user manager
//! is normally torn down when the last session for that user ends, so a gateway
//! that is meant to answer a phone while nobody is logged in needs
//! `loginctl enable-linger`. It is attempted and reported, never required: it
//! is the one step a hardened host may refuse, and refusing it costs autostart
//! on a headless box, not the install.

use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result};

/// The reverse-DNS name the LaunchAgent is registered under. Also the systemd
/// unit's stem, so one machine reads the same either way.
pub const SERVICE_LABEL: &str = "dev.osuki.muqun-gateway";

/// What `service status` found.
pub enum ServiceState {
    /// Registered with the init system, and it reports the gateway as loaded.
    Installed,
    /// A unit file is on disk but the init system does not have it loaded,
    /// which is what a half-finished install or a manual `bootout` leaves.
    FileOnly,
    NotInstalled,
}

/// Everything the unit file needs to name. Passed in rather than resolved here
/// so this module stays free of the config-directory rules in `main`.
pub struct ServicePaths {
    pub exe: PathBuf,
    pub config: PathBuf,
    pub log: PathBuf,
    /// The account the unit is pinned to. See `launch_agent_plist`.
    pub home: PathBuf,
}

pub fn install(paths: &ServicePaths) -> Result<()> {
    let unit = unit_path()?;
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&unit, unit_contents(paths))
        .with_context(|| format!("failed to write {}", unit.display()))?;
    enable(&unit)?;
    println!("service installed: {}", unit.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let unit = unit_path()?;
    disable(&unit);
    if unit.exists() {
        std::fs::remove_file(&unit)
            .with_context(|| format!("failed to remove {}", unit.display()))?;
        println!("service removed: {}", unit.display());
    } else {
        println!("no service was installed");
    }
    #[cfg(not(target_os = "macos"))]
    run_quiet("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

pub fn state() -> Result<ServiceState> {
    let unit = unit_path()?;
    if !unit.exists() {
        return Ok(ServiceState::NotInstalled);
    }
    Ok(if loaded() {
        ServiceState::Installed
    } else {
        ServiceState::FileOnly
    })
}

/// Whether an init system is currently managing the gateway.
///
/// The caller uses this to explain why killing the pid did not stick: with a
/// service installed, `KeepAlive`/`Restart=always` puts it straight back.
pub fn is_installed() -> bool {
    matches!(state(), Ok(ServiceState::Installed))
}

pub fn unit_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("failed to locate the home directory")?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents")
            .join(format!("{SERVICE_LABEL}.plist"))
    } else {
        home.join(".config/systemd/user")
            .join(format!("{SERVICE_LABEL}.service"))
    })
}

// ---------------------------------------------------------------- unit files

fn unit_contents(paths: &ServicePaths) -> String {
    if cfg!(target_os = "macos") {
        launch_agent_plist(paths)
    } else {
        systemd_unit(paths)
    }
}

/// `KeepAlive` rather than `KeepAlive/SuccessfulExit`: a gateway that exits for
/// any reason -- a crash, a port that came back busy, an OOM -- should come
/// back, and there is no exit of its own that means "stay down".
///
/// `ProcessType Background` keeps it out of the foreground scheduling band it
/// has no business in; it serves a phone, not a window.
///
/// `HOME` is pinned for the same reason the config path is. Only the config is
/// passed as an argument; the *state* directory -- the devices list, the lock,
/// the pid file -- is resolved at runtime from the environment, so leaving it
/// to whatever the init system hands the process makes the unit mean different
/// things in different environments. launchd and systemd both set `HOME` today,
/// and the failure mode when one does not is silent: the gateway comes up
/// against a different state directory, finds no paired devices, and the phone
/// simply cannot reach it with nothing anywhere saying why. Observed exactly
/// once, in a test whose installer ran under an overridden `HOME` while launchd
/// started the agent under the real one.
fn launch_agent_plist(paths: &ServicePaths) -> String {
    let exe = xml(&paths.exe);
    let config = xml(&paths.config);
    let log = xml(&paths.log);
    let home = xml(&paths.home);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{SERVICE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>run</string>
    <string>--config</string>
    <string>{config}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home}</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// `Restart=always` with a delay, and `default.target` rather than
/// `multi-user.target`: this is a user unit, and `default.target` is the one a
/// user manager actually reaches on login.
///
/// Output is left to the journal instead of being redirected at the log file
/// the way the plist does it -- systemd already captures stdout, and pointing
/// two writers at one file is how a log ends up interleaved mid-line.
fn systemd_unit(paths: &ServicePaths) -> String {
    let exe = paths.exe.display();
    let config = paths.config.display();
    let home = paths.home.display();
    format!(
        "[Unit]\n\
         Description=Muqun Gateway\n\
         Documentation=https://github.com/osuki-dev/muqun-gateway\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Environment=HOME={home}\n\
         ExecStart={exe} run --config {config}\n\
         Restart=always\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Paths reach the plist as XML text, and a home directory may legally contain
/// `&` or `<`. Unescaped, one of those does not misrender -- it makes the file
/// unparseable, and `launchctl` rejects the whole agent.
fn xml(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ------------------------------------------------------------- init plumbing

#[cfg(target_os = "macos")]
fn enable(unit: &Path) -> Result<()> {
    let domain = gui_domain();
    // Reinstalling over a live agent: bootstrap refuses a label that is already
    // loaded, so the old one goes first. A failure here is the ordinary "it was
    // not loaded" case, which is why it is not checked.
    run_quiet(
        "launchctl",
        &["bootout", &format!("{domain}/{SERVICE_LABEL}")],
    );
    let status = ProcessCommand::new("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(unit)
        .status()
        .context("failed to run launchctl bootstrap")?;
    anyhow::ensure!(status.success(), "launchctl bootstrap failed ({status})");
    Ok(())
}

#[cfg(target_os = "macos")]
fn disable(_unit: &Path) {
    run_quiet(
        "launchctl",
        &["bootout", &format!("{}/{SERVICE_LABEL}", gui_domain())],
    );
}

#[cfg(target_os = "macos")]
fn loaded() -> bool {
    run_quiet(
        "launchctl",
        &["print", &format!("{}/{SERVICE_LABEL}", gui_domain())],
    )
}

#[cfg(target_os = "macos")]
fn gui_domain() -> String {
    // The per-user GUI domain, which is where an agent that has to reach the
    // user's own tmux server belongs. `unsafe` only because getuid is FFI; it
    // cannot fail and touches nothing.
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(not(target_os = "macos"))]
fn enable(_unit: &Path) -> Result<()> {
    run_quiet("systemctl", &["--user", "daemon-reload"]);
    let unit_name = format!("{SERVICE_LABEL}.service");
    let status = ProcessCommand::new("systemctl")
        .args(["--user", "enable", "--now", &unit_name])
        .status()
        .context("failed to run systemctl --user enable")?;
    anyhow::ensure!(
        status.success(),
        "systemctl --user enable failed ({status})"
    );

    // Without lingering, the user manager -- and the gateway with it -- is torn
    // down when the last session ends, so a phone can reach the machine only
    // while somebody happens to be logged in. Reported rather than enforced:
    // this is the step a locked-down host may refuse, and refusing it costs
    // autostart while logged out, not the install.
    if !run_quiet("loginctl", &["enable-linger"]) {
        println!(
            "note: `loginctl enable-linger` did not succeed. The gateway will run while you are\n\
             logged in, but not after you log out. Run it yourself, or ask an administrator."
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn disable(_unit: &Path) {
    let unit_name = format!("{SERVICE_LABEL}.service");
    run_quiet("systemctl", &["--user", "disable", "--now", &unit_name]);
}

#[cfg(not(target_os = "macos"))]
fn loaded() -> bool {
    let unit_name = format!("{SERVICE_LABEL}.service");
    run_quiet(
        "systemctl",
        &["--user", "is-enabled", "--quiet", &unit_name],
    )
}

fn run_quiet(program: &str, args: &[&str]) -> bool {
    ProcessCommand::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> ServicePaths {
        ServicePaths {
            exe: PathBuf::from("/home/a b/.local/bin/muqun-gateway"),
            config: PathBuf::from("/home/a b/.config/muqun-gateway/config.json"),
            log: PathBuf::from("/home/a b/.local/share/muqun-gateway/gateway.log"),
            home: PathBuf::from("/home/a b"),
        }
    }

    #[test]
    fn plist_names_the_binary_the_config_and_the_log() {
        let plist = launch_agent_plist(&paths());
        assert!(plist.contains("<string>/home/a b/.local/bin/muqun-gateway</string>"));
        assert!(plist.contains("<string>/home/a b/.config/muqun-gateway/config.json</string>"));
        assert!(plist.contains("<string>/home/a b/.local/share/muqun-gateway/gateway.log</string>"));
        assert!(plist.contains(SERVICE_LABEL));
    }

    #[test]
    fn plist_starts_at_login_and_comes_back_after_a_crash() {
        // The two keys that are the whole point of the file. A plist that
        // parses but carries neither is an install that silently does nothing.
        let plist = launch_agent_plist(&paths());
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
    }

    #[test]
    fn a_home_directory_with_xml_in_it_still_parses() {
        // `&` in a path is legal and would otherwise make launchctl reject the
        // whole agent rather than misdraw one line of it.
        let plist = launch_agent_plist(&ServicePaths {
            exe: PathBuf::from("/Users/a&b/bin/muqun-gateway"),
            config: PathBuf::from("/Users/a&b/config.json"),
            log: PathBuf::from("/Users/a<b>/gateway.log"),
            home: PathBuf::from("/Users/a&b"),
        });
        assert!(plist.contains("/Users/a&amp;b/bin/muqun-gateway"));
        assert!(!plist.contains("/Users/a&b/bin"));
        assert!(plist.contains("/Users/a&lt;b&gt;/gateway.log"));
    }

    #[test]
    fn systemd_unit_restarts_and_installs_into_the_user_target() {
        let unit = systemd_unit(&paths());
        assert!(unit.contains("ExecStart=/home/a b/.local/bin/muqun-gateway run --config /home/a b/.config/muqun-gateway/config.json"));
        assert!(unit.contains("Restart=always"));
        // `default.target`, not `multi-user.target`: a user manager reaches the
        // former on login and never the latter.
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn both_units_pin_the_account_they_were_installed_for() {
        // Only the config path is passed as an argument; the state directory --
        // devices, lock, pid -- is resolved from the environment at runtime. An
        // unpinned unit therefore means a different thing under a different
        // environment, and it fails silently: no devices, no explanation.
        let plist = launch_agent_plist(&paths());
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>HOME</key>"));
        assert!(plist.contains("<string>/home/a b</string>"));
        assert!(systemd_unit(&paths()).contains("Environment=HOME=/home/a b"));
    }

    #[test]
    fn the_unit_lives_under_the_users_own_home() {
        // The claim the installer makes to the reader -- nothing system-wide,
        // no administrator password -- is only true if this stays inside $HOME.
        let unit = unit_path().expect("home directory");
        let home = dirs::home_dir().expect("home directory");
        assert!(
            unit.starts_with(&home),
            "{} escaped {}",
            unit.display(),
            home.display()
        );
    }
}
