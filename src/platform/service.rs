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
    /// The `PATH` the supervised gateway runs with. See `launch_agent_plist`.
    pub path: String,
    /// The character encoding it runs with, for the same reason.
    pub lc_ctype: String,
}

pub fn install(paths: &ServicePaths) -> Result<()> {
    // Validate before creating directories or replacing an existing unit.
    let contents = unit_contents(paths)?;
    let unit = unit_path()?;
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&unit, contents)
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

fn unit_contents(paths: &ServicePaths) -> Result<String> {
    if cfg!(target_os = "macos") {
        Ok(launch_agent_plist(paths))
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
///
/// `PATH` and `LC_CTYPE` are pinned for the same reason and were missed for
/// longer. launchd hands a user agent `/usr/bin:/bin:/usr/sbin:/sbin` and no
/// locale at all, and both of those break the tmux backend on their own:
///
/// - `/opt/homebrew/bin` is not on that `PATH`, so a Homebrew tmux cannot be
///   spawned, and the backend reports itself unavailable with tmux plainly
///   running two feet away.
/// - With no UTF-8 locale, tmux replaces every byte it will not print with `_`
///   -- including the `\u{1f}` the adapter joins its `-F` fields with, so every
///   list parse fails, and every non-ASCII pane title along with it.
///
/// `setup` catches neither, because `setup` runs in the user's shell and the
/// agent does not. Anything else spawned by name has the same exposure: `git`
/// for worktrees, and every agent executable in the catalog.
fn launch_agent_plist(paths: &ServicePaths) -> String {
    let exe = xml(&paths.exe);
    let config = xml(&paths.config);
    let log = xml(&paths.log);
    let home = xml(&paths.home);
    let path = xml_text(&paths.path);
    let lc_ctype = xml_text(&paths.lc_ctype);
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
  <key>AbandonProcessGroup</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>{home}</string>
    <key>PATH</key>
    <string>{path}</string>
    <key>LC_CTYPE</key>
    <string>{lc_ctype}</string>
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
fn systemd_unit(paths: &ServicePaths) -> Result<String> {
    let path_text = |path: &Path| -> Result<String> {
        Ok(path
            .to_str()
            .context("systemd service paths must be valid UTF-8")?
            .to_owned())
    };
    let exe = systemd_word(&format!(":{}", path_text(&paths.exe)?))?;
    let config = systemd_word(&path_text(&paths.config)?)?;
    let home = systemd_word(&format!("HOME={}", path_text(&paths.home)?))?;
    let path = systemd_word(&format!("PATH={}", paths.path))?;
    let lc_ctype = systemd_word(&format!("LC_CTYPE={}", paths.lc_ctype))?;
    Ok(format!(
        "[Unit]\n\
         Description=Muqun Gateway\n\
         Documentation=https://github.com/osuki-dev/muqun-gateway\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Environment={home}\n\
         Environment={path}\n\
         Environment={lc_ctype}\n\
         ExecStart={exe} run --config {config}\n\
         Restart=always\n\
         RestartSec=3\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

/// systemd.syntax quoting is not shell quoting. Both directives expand `%`
/// specifiers. Environment leaves `$` literal; ExecStart's `:` prefix disables
/// variable expansion explicitly, including argv[0], so literal dollar signs
/// in executable paths and arguments agree (systemd.service/systemd.exec).
/// Reject controls instead of allowing injected directives.
fn systemd_word(value: &str) -> Result<String> {
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "systemd service values must not contain control characters"
    );
    let mut quoted = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

/// Paths reach the plist as XML text, and a home directory may legally contain
/// `&` or `<`. Unescaped, one of those does not misrender -- it makes the file
/// unparseable, and `launchctl` rejects the whole agent.
fn xml(path: &Path) -> String {
    xml_text(&path.display().to_string())
}

/// The same escaping for a value that was never a path -- `PATH` is a list of
/// them, and one entry containing `&` would take the whole agent down with it.
fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ------------------------------------------------------------- init plumbing

/// How long a reinstall waits for the old agent to be fully gone. Past launchd's
/// own `ExitTimeOut` (5 s by default, when SIGTERM becomes SIGKILL), with room.
#[cfg(target_os = "macos")]
const BOOTOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Bootstrap attempts, a second apart, before a reinstall gives up.
#[cfg(target_os = "macos")]
const BOOTSTRAP_ATTEMPTS: u32 = 10;

#[cfg(target_os = "macos")]
fn enable(unit: &Path) -> Result<()> {
    let domain = gui_domain();
    let target = format!("{domain}/{SERVICE_LABEL}");
    // Reinstalling over a live agent: bootstrap refuses a label that is already
    // loaded, so the old one goes first. A failure here is the ordinary "it was
    // not loaded" case, which is why it is not checked.
    //
    // `bootout` can return while launchd is still tearing the old job down, and
    // a bootstrap in that window fails with "5: Input/output error" -- which
    // is how the installer's update step left the gateway stopped on v0.12.1.
    // So wait until the label is released and the old process has exited (it
    // holds the state lock, and a replacement started beside it would lose that
    // race and exit), then bootstrap, retrying while launchd catches up.
    let old_pid = loaded_pid(&target);
    run_quiet("launchctl", &["bootout", &target]);
    wait_until(BOOTOUT_WAIT, || {
        !run_quiet("launchctl", &["print", &target]) && !old_pid.is_some_and(process_alive)
    });

    let mut last_error = String::new();
    for attempt in 1..=BOOTSTRAP_ATTEMPTS {
        let output = ProcessCommand::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(unit)
            .stdin(Stdio::null())
            .output()
            .context("failed to run launchctl bootstrap")?;
        if output.status.success() {
            return Ok(());
        }
        last_error = format!(
            "{} ({})",
            String::from_utf8_lossy(&output.stderr).trim(),
            output.status
        );
        // Only 5 (EIO) is launchd still holding the old job. Anything else --
        // a unit it will not parse, no GUI session to load into -- will not
        // change by waiting, so report it now.
        if output.status.code() != Some(5) {
            anyhow::bail!("launchctl bootstrap failed: {last_error}");
        }
        if attempt < BOOTSTRAP_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    anyhow::bail!(
        "launchctl bootstrap failed after {BOOTSTRAP_ATTEMPTS} attempts: {last_error}\n\
         launchd has not released the previous gateway yet, and the gateway is not running. \
         Try `muqun-gateway service install` again in a moment."
    )
}

/// The pid launchd reports for a loaded job, if it is running one.
#[cfg(target_os = "macos")]
fn loaded_pid(target: &str) -> Option<u32> {
    let output = ProcessCommand::new("launchctl")
        .args(["print", target])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_launchd_pid(&String::from_utf8_lossy(&output.stdout))
}

/// The job's own `pid = N` line. Only the top-level one: nested sections
/// (endpoints, spawn info) are indented further and are not the job's pid.
#[cfg(any(target_os = "macos", test))]
fn parse_launchd_pid(print: &str) -> Option<u32> {
    print
        .lines()
        .find_map(|line| line.strip_prefix("\tpid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

#[cfg(target_os = "macos")]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the pid exists and may be signalled.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
fn wait_until(limit: std::time::Duration, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    while !done() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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

    #[test]
    fn the_job_pid_is_read_from_its_own_line_and_nowhere_else() {
        let print = "gui/501/dev.osuki.muqun-gateway = {\n\
                     \tactive count = 1\n\
                     \tstate = running\n\
                     \tendpoints = {\n\
                     \t\tpid = 7\n\
                     \t}\n\
                     \tpid = 99542\n\
                     }\n";
        assert_eq!(parse_launchd_pid(print), Some(99542));
        assert_eq!(parse_launchd_pid("\tstate = not running\n"), None);
    }

    fn paths() -> ServicePaths {
        ServicePaths {
            exe: PathBuf::from("/home/a b/.local/bin/muqun-gateway"),
            config: PathBuf::from("/home/a b/.config/muqun-gateway/config.json"),
            log: PathBuf::from("/home/a b/.local/share/muqun-gateway/gateway.log"),
            home: PathBuf::from("/home/a b"),
            path: String::from("/opt/homebrew/bin:/usr/bin:/bin"),
            lc_ctype: String::from("zh_CN.UTF-8"),
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
            path: String::from("/Users/a&b/bin:/usr/bin"),
            lc_ctype: String::from("UTF-8"),
        });
        assert!(plist.contains("/Users/a&amp;b/bin/muqun-gateway"));
        assert!(!plist.contains("/Users/a&b/bin"));
        assert!(plist.contains("/Users/a&lt;b&gt;/gateway.log"));
        assert!(plist.contains("<string>/Users/a&amp;b/bin:/usr/bin</string>"));
    }

    #[test]
    fn both_units_carry_the_environment_the_gateway_has_to_spawn_with() {
        // The two bugs this exists to prevent, both from launchd handing a user
        // agent an environment the user never sees: a `PATH` a Homebrew tmux
        // lives outside of, so the backend reports itself unavailable forever
        // while tmux is plainly running; and no locale at all, under which tmux
        // replaces the `\u{1f}` field separator with `_` and every list parse
        // fails. `setup` catches neither -- setup runs in the user's shell, the
        // agent does not.
        let plist = launch_agent_plist(&paths());
        assert!(plist.contains("<key>AbandonProcessGroup</key>"));
        assert!(systemd_unit(&paths()).unwrap().contains("KillMode=process"));
        assert!(plist.contains("<key>PATH</key>"));
        assert!(plist.contains("<string>/opt/homebrew/bin:/usr/bin:/bin</string>"));
        assert!(plist.contains("<key>LC_CTYPE</key>"));
        assert!(plist.contains("<string>zh_CN.UTF-8</string>"));
        // Quoted on the systemd side, because a PATH entry may contain a space
        // and an unquoted `Environment=` truncates at it rather than failing.
        let unit = systemd_unit(&paths()).unwrap();
        assert!(unit.contains("Environment=\"PATH=/opt/homebrew/bin:/usr/bin:/bin\""));
        assert!(unit.contains("Environment=\"LC_CTYPE=zh_CN.UTF-8\""));
    }

    #[test]
    fn systemd_unit_restarts_and_installs_into_the_user_target() {
        let unit = systemd_unit(&paths()).unwrap();
        assert!(unit.contains("ExecStart=\":/home/a b/.local/bin/muqun-gateway\" run --config \"/home/a b/.config/muqun-gateway/config.json\""));
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
        assert!(systemd_unit(&paths())
            .unwrap()
            .contains("Environment=\"HOME=/home/a b\""));
    }

    #[test]
    fn systemd_escapes_commands_and_environment_with_distinct_dollar_rules() {
        let value = "/home/a b/\"quoted\"/back\\slash/$HOME/${USER}/%h/单引号'";
        let escaped = "/home/a b/\\\"quoted\\\"/back\\\\slash/$HOME/${USER}/%%h/单引号'";
        let mut paths = paths();
        paths.exe = value.into();
        paths.config = value.into();
        paths.home = value.into();
        paths.path = value.into();
        paths.lc_ctype = value.into();
        let unit = systemd_unit(&paths).unwrap();
        assert!(unit.contains(&format!(
            "ExecStart=\":{escaped}\" run --config \"{escaped}\"\n"
        )));
        for key in ["HOME", "PATH", "LC_CTYPE"] {
            assert!(unit.contains(&format!("Environment=\"{key}={escaped}\"\n")));
        }
        assert_eq!(systemd_word("").unwrap(), "\"\"");
        assert_eq!(systemd_word("\\").unwrap(), "\"\\\\\"");
    }

    #[test]
    fn systemd_rejects_controls_in_every_interpolated_field() {
        for control in ['\n', '\r', '\0', '\t', '\u{7f}'] {
            for field in 0..5 {
                let mut paths = paths();
                let invalid = format!("/safe{control}ExecStart=/unwanted");
                match field {
                    0 => paths.exe = invalid.into(),
                    1 => paths.config = invalid.into(),
                    2 => paths.home = invalid.into(),
                    3 => paths.path = invalid,
                    _ => paths.lc_ctype = invalid,
                }
                assert!(systemd_unit(&paths).is_err(), "field {field}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn systemd_rejects_non_utf8_paths_instead_of_changing_their_identity() {
        use std::os::unix::ffi::OsStringExt;
        let mut paths = paths();
        paths.exe = std::ffi::OsString::from_vec(b"/bin/invalid-\xff".to_vec()).into();
        assert!(systemd_unit(&paths).is_err());
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
