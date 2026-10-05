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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub state: PathBuf,
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
    enable(&unit, paths)?;
    println!("service installed: {}", unit.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let unit = unit_path()?;
    disable(&unit)?;
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

/// Refuse to control a same-label service belonging to another installation.
/// Linux checks the reloaded effective unit, not just its base file. Older units
/// without directory pins also require inspecting the inherited manager environment.
pub fn ensure_current_install(paths: &ServicePaths) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let contents = std::fs::read_to_string(unit_path()?)?;
        validate_install_unit(&contents, paths)?;
        // The file is what loads at the next login; what launchd runs now is
        // whatever it loaded, which an edited plist does not change.
        if let Some(print) = super::launchd::Launchctl::system()?.print() {
            validate_loaded_program(&print, &paths.exe)?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    ensure_effective_unit(&format!("{SERVICE_LABEL}.service"), paths)
}

#[cfg(any(target_os = "macos", test))]
fn validate_install_unit(contents: &str, paths: &ServicePaths) -> Result<()> {
    if cfg!(target_os = "macos") {
        validate_launch_agent_program(contents, &paths.exe)?;
    }
    let config_matches = if cfg!(target_os = "macos") {
        contents.contains(&format!("<string>{}</string>", xml(&paths.config)))
    } else {
        contents.contains(&format!(
            "--config {}",
            systemd_word(&paths.config.display().to_string())?
        ))
    };
    let state_matches = if contents.contains("MUQUN_GATEWAY_STATE_DIR") {
        if cfg!(target_os = "macos") {
            contents.contains(&format!("<string>{}</string>", xml(&paths.state)))
        } else {
            contents.contains(&systemd_word(&format!(
                "MUQUN_GATEWAY_STATE_DIR={}",
                paths.state.display()
            ))?)
        }
    } else {
        let default = if cfg!(target_os = "macos") {
            paths.home.join("Library/Application Support/muqun-gateway")
        } else {
            paths.home.join(".local/share/muqun-gateway")
        };
        paths.state == default
    };
    anyhow::ensure!(config_matches && state_matches,
        "the installed service uses different config/state paths; refusing to control another installation");
    anyhow::ensure!(if cfg!(target_os = "macos") {
        contents.contains("<key>AbandonProcessGroup</key>\n  <true/>")
    } else { contents.contains("KillMode=process") || contents.contains("KillMode=none") },
        "the service may terminate terminal tasks; update its child-process lifetime rules before controlling it");
    Ok(())
}

/// The plist runs this binary: one `ProgramArguments`, no `Program`, and
/// `[<exe>, "run", ...]` where `<exe>` is the same file as `exe` however
/// either is spelled.
#[cfg(any(target_os = "macos", test))]
fn validate_launch_agent_program(contents: &str, exe: &Path) -> Result<()> {
    anyhow::ensure!(
        !contents.contains("<key>Program</key>")
            && contents.matches("<key>ProgramArguments</key>").count() == 1,
        "ambiguous LaunchAgent executable; reinstall the service from the standalone binary"
    );
    let arguments = plist_program_arguments(contents).unwrap_or_default();
    anyhow::ensure!(
        arguments.get(1).is_some_and(|run| run == "run")
            && arguments
                .first()
                .is_some_and(|program| same_executable(Path::new(program), exe)),
        "the installed LaunchAgent runs {}, not this binary ({}); refusing to control another installation",
        arguments.first().map_or("an unreadable program", String::as_str),
        exe.display()
    );
    Ok(())
}

/// What launchd has loaded must be this binary too. `launchctl print` names
/// the job's executable on its own top-level `program = ` line.
#[cfg(any(target_os = "macos", test))]
fn validate_loaded_program(print: &str, exe: &Path) -> Result<()> {
    let program = print
        .lines()
        .find_map(|line| line.strip_prefix("\tprogram = "))
        .map(str::trim);
    anyhow::ensure!(
        program.is_some_and(|program| same_executable(Path::new(program), exe)),
        "launchd has the gateway loaded from {}, not this binary ({}); refusing to control another installation",
        program.unwrap_or("an unknown program"),
        exe.display()
    );
    Ok(())
}

/// Two spellings of one file: a symlink, `./muqun-gateway`, `bin/../bin/x`.
/// A path that does not resolve matches only itself.
#[cfg(any(target_os = "macos", test))]
fn same_executable(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// `ProgramArguments`' strings, unescaped. `None` when the key is not followed
/// by an array.
#[cfg(any(target_os = "macos", test))]
fn plist_program_arguments(contents: &str) -> Option<Vec<String>> {
    let (_, after) = contents.split_once("<key>ProgramArguments</key>")?;
    let (array, _) = after
        .trim_start()
        .strip_prefix("<array>")?
        .split_once("</array>")?;
    let mut arguments = Vec::new();
    let mut rest = array;
    while let Some((_, tail)) = rest.split_once("<string>") {
        let (value, tail) = tail.split_once("</string>")?;
        arguments.push(
            value
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        );
        rest = tail;
    }
    Some(arguments)
}

/// Reload before inspecting: a later reload between validation and stop would
/// activate unchecked drop-ins. D-Bus provides the exact argv/environment;
/// systemctl's human-readable ExecStart joins argv with spaces and is lossy.
#[cfg(not(target_os = "macos"))]
pub(crate) fn ensure_effective_unit(unit: &str, paths: &ServicePaths) -> Result<()> {
    checked_command("systemctl", &["--user", "daemon-reload"])?;
    let policy = command_output(
        "systemctl",
        &["--user", "show", unit, "--property=KillMode", "--value"],
    )?;
    let object = command_json(
        "busctl",
        &[
            "--user",
            "--json=short",
            "--timeout=5s",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "GetUnit",
            "s",
            unit,
        ],
    )?;
    let object = object
        .get("data")
        .and_then(|data| data.get(0))
        .and_then(serde_json::Value::as_str)
        .filter(|path| path.starts_with("/org/freedesktop/systemd1/unit/"))
        .context("invalid systemd unit object")?;
    let properties = command_json(
        "busctl",
        &[
            "--user",
            "--json=short",
            "--timeout=5s",
            "call",
            "org.freedesktop.systemd1",
            object,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            "s",
            "org.freedesktop.systemd1.Service",
        ],
    )?;
    let assignments: Vec<String> =
        serde_json::from_value(properties["data"][0]["Environment"]["data"].clone())
            .map_err(|_| anyhow::anyhow!("missing or invalid effective service environment"))?;
    let pinned = [
        "HOME=",
        "MUQUN_GATEWAY_CONFIG_DIR=",
        "MUQUN_GATEWAY_STATE_DIR=",
    ]
    .iter()
    .all(|name| assignments.iter().any(|value| value.starts_with(name)));
    let manager_environment = if pinned {
        Vec::new()
    } else {
        let manager = command_json(
            "busctl",
            &[
                "--user",
                "--json=short",
                "--timeout=5s",
                "call",
                "org.freedesktop.systemd1",
                "/org/freedesktop/systemd1",
                "org.freedesktop.DBus.Properties",
                "GetAll",
                "s",
                "org.freedesktop.systemd1.Manager",
            ],
        )?;
        serde_json::from_value(manager["data"][0]["Environment"]["data"].clone())
            .map_err(|_| anyhow::anyhow!("missing or invalid inherited manager environment"))?
    };
    validate_effective_unit(&properties, policy.trim(), paths, &manager_environment)
}

#[cfg(not(target_os = "macos"))]
fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to inspect effective service with {program}"))?;
    // Never include Environment/JSON output in diagnostics: it may contain secrets.
    anyhow::ensure!(
        output.status.success(),
        "{program} could not inspect effective service configuration ({})",
        output.status
    );
    String::from_utf8(output.stdout).context("invalid UTF-8 service configuration")
}

#[cfg(not(target_os = "macos"))]
fn command_json(program: &str, args: &[&str]) -> Result<serde_json::Value> {
    serde_json::from_str(&command_output(program, args)?)
        .context("invalid structured service configuration")
}

#[cfg(not(target_os = "macos"))]
fn validate_effective_unit(
    reply: &serde_json::Value,
    policy: &str,
    paths: &ServicePaths,
    manager_environment: &[String],
) -> Result<()> {
    let properties = reply
        .get("data")
        .and_then(|data| data.get(0))
        .and_then(serde_json::Value::as_object)
        .context("missing effective systemd properties")?;
    let property = |name: &str| -> Result<&serde_json::Value> {
        properties
            .get(name)
            .and_then(|value| value.get("data"))
            .with_context(|| format!("missing effective systemd {name}"))
    };
    anyhow::ensure!(
        matches!(policy, "process" | "none") && property("KillMode")?.as_str() == Some(policy),
        "the effective service may terminate terminal tasks; require KillMode=process or none"
    );
    for name in [
        "EnvironmentFiles",
        "UnsetEnvironment",
        "ExecStop",
        "ExecStopPost",
    ] {
        anyhow::ensure!(property(name)?.as_array().is_some_and(Vec::is_empty),
            "effective {name} prevents verifying service ownership/shutdown; refusing lifecycle action");
    }
    let starts = property("ExecStart")?
        .as_array()
        .context("invalid effective ExecStart")?;
    anyhow::ensure!(
        starts.len() == 1,
        "require exactly one effective gateway ExecStart"
    );
    let executable = starts[0]
        .get(0)
        .and_then(serde_json::Value::as_str)
        .context("missing effective executable")?;
    let argv: Vec<String> =
        serde_json::from_value(starts[0].get(1).context("missing effective argv")?.clone())?;
    let config_arg = match argv.as_slice() {
        [_, run, flag, config] if run == "run" && flag == "--config" => Some(config.as_str()),
        [_, run, flag] if run == "run" => flag.strip_prefix("--config="),
        _ => None,
    };
    anyhow::ensure!(
        Path::new(executable) == paths.exe
            && argv.first().is_some_and(|arg| Path::new(arg) == paths.exe)
            && config_arg.is_some_and(|arg| Path::new(arg) == paths.config),
        "effective ExecStart belongs to another installation; refusing lifecycle action"
    );
    // ExecStart's argv is before environment expansion. Literal '$' in paths
    // is safe only with the ':' (no-env-expand) flag, exposed by ExecStartEx.
    if argv.iter().any(|arg| arg.contains('$')) {
        let flags = property("ExecStartEx")?
            .get(0)
            .and_then(|start| start.get(2))
            .and_then(serde_json::Value::as_array)
            .context("missing effective ExecStart flags")?;
        anyhow::ensure!(flags.iter().any(|flag| flag.as_str() == Some("no-env-expand")),
            "effective ExecStart expands environment variables; refusing ambiguous installation identity");
    }
    let assignments: Vec<String> = serde_json::from_value(property("Environment")?.clone())
        .map_err(|_| anyhow::anyhow!("invalid effective service environment"))?;
    let mut environment = std::collections::HashMap::new();
    for assignment in manager_environment {
        let (name, value) = assignment
            .split_once('=')
            .context("invalid manager environment assignment")?;
        environment.insert(name, value);
    }
    let mut unit_names = std::collections::HashSet::new();
    for assignment in &assignments {
        let (name, value) = assignment
            .split_once('=')
            .context("invalid effective environment assignment")?;
        anyhow::ensure!(
            unit_names.insert(name),
            "ambiguous effective service environment"
        );
        environment.insert(name, value);
    }
    anyhow::ensure!(
        environment
            .get("HOME")
            .is_some_and(|value| Path::new(value) == paths.home),
        "effective HOME belongs to another installation; refusing lifecycle action"
    );
    // Mirror only the directory precedence in store: explicit pins, then the
    // legacy plugin environment before import, then HOME/XDG defaults. Read the
    // marker without calling the migrating store helpers or mutating this CLI's environment.
    let config_parent = environment
        .get("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.home.join(".config"));
    let standalone_config = config_parent.join("muqun-gateway");
    let imported = standalone_config
        .join(crate::HERDR_PLUGIN_IMPORT_MARKER)
        .exists();
    let resolve = |pin: &str, legacy: &str, default: PathBuf| {
        environment
            .get(pin)
            .or_else(|| {
                if imported {
                    None
                } else {
                    environment.get(legacy)
                }
            })
            .map(PathBuf::from)
            .unwrap_or(default)
    };
    let config = resolve(
        "MUQUN_GATEWAY_CONFIG_DIR",
        "HERDR_PLUGIN_CONFIG_DIR",
        standalone_config,
    );
    let data_parent = environment
        .get("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.home.join(".local/share"));
    let state = resolve(
        "MUQUN_GATEWAY_STATE_DIR",
        "HERDR_PLUGIN_STATE_DIR",
        data_parent.join("muqun-gateway"),
    );
    anyhow::ensure!(
        config.is_absolute()
            && state.is_absolute()
            && config == paths.config.parent().context("config has no parent")?
            && state == paths.state,
        "effective config/state paths belong to another installation; refusing lifecycle action"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

impl ServiceAction {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn control(action: ServiceAction) -> Result<()> {
    checked_command(
        "systemctl",
        &["--user", action.verb(), &format!("{SERVICE_LABEL}.service")],
    )
}

/// Every macOS transition shares `launchd`'s retrying bootstrap; see there.
#[cfg(target_os = "macos")]
pub fn control(action: ServiceAction) -> Result<()> {
    super::launchd::Launchctl::system()?.control(action)
}

pub fn checked_command(program: &str, args: &[&str]) -> Result<()> {
    let output = ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} {} failed ({}): {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(512)
            .collect::<String>()
            .trim()
    );
    Ok(())
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
    let config_dir = xml(paths.config.parent().unwrap_or(Path::new(".")));
    let state = xml(&paths.state);
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
    <key>MUQUN_GATEWAY_CONFIG_DIR</key>
    <string>{config_dir}</string>
    <key>MUQUN_GATEWAY_STATE_DIR</key>
    <string>{state}</string>
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
    let config_dir = systemd_word(&format!(
        "MUQUN_GATEWAY_CONFIG_DIR={}",
        path_text(paths.config.parent().context("config has no parent")?)?
    ))?;
    let state = systemd_word(&format!(
        "MUQUN_GATEWAY_STATE_DIR={}",
        path_text(&paths.state)?
    ))?;
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
         Environment={config_dir}\n\
         Environment={state}\n\
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

#[cfg(target_os = "macos")]
fn enable(_unit: &Path, _paths: &ServicePaths) -> Result<()> {
    // Reinstalling over a live agent: bootstrap refuses a label that is already
    // loaded, so the old one goes first, and the bootstrap retries while
    // launchd releases it (see `launchd`).
    super::launchd::Launchctl::system()?.reload().context(
        "the gateway is not running. Try `muqun-gateway service install` again in a moment",
    )
}

#[cfg(target_os = "macos")]
pub fn owns_pid(pid: u32) -> bool {
    super::launchd::Launchctl::system().is_ok_and(|launchd| launchd.loaded_pid() == Some(pid))
}

#[cfg(target_os = "macos")]
fn disable(_unit: &Path) -> Result<()> {
    control(ServiceAction::Stop)
}

#[cfg(target_os = "macos")]
fn loaded() -> bool {
    super::launchd::Launchctl::system().is_ok_and(|launchd| launchd.loaded())
}

#[cfg(not(target_os = "macos"))]
fn enable(_unit: &Path, paths: &ServicePaths) -> Result<()> {
    let unit_name = format!("{SERVICE_LABEL}.service");
    ensure_effective_unit(&unit_name, paths)?;
    let status = ProcessCommand::new("systemctl")
        .args(["--user", "enable", &unit_name])
        .stdin(Stdio::null())
        .status()
        .context("failed to run systemctl --user enable")?;
    anyhow::ensure!(
        status.success(),
        "systemctl --user enable failed ({status})"
    );

    checked_command("systemctl", &["--user", "restart", &unit_name])?;

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
fn disable(_unit: &Path) -> Result<()> {
    let unit_name = format!("{SERVICE_LABEL}.service");
    checked_command("systemctl", &["--user", "disable", "--now", &unit_name])
}

#[cfg(not(target_os = "macos"))]
fn loaded() -> bool {
    let unit_name = format!("{SERVICE_LABEL}.service");
    run_quiet(
        "systemctl",
        &["--user", "is-enabled", "--quiet", &unit_name],
    )
}

#[cfg(not(target_os = "macos"))]
fn run_quiet(program: &str, args: &[&str]) -> bool {
    ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::null())
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
            state: PathBuf::from("/home/a b/.local/share/muqun-gateway"),
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
            state: PathBuf::from("/Users/a&b/state"),
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
    fn units_pin_resolved_config_and_state_overrides_and_refuse_other_installs() {
        let fixture = paths();
        let unit = unit_contents(&fixture).unwrap();
        assert!(unit.contains("MUQUN_GATEWAY_CONFIG_DIR"));
        assert!(unit.contains("MUQUN_GATEWAY_STATE_DIR"));
        assert!(validate_install_unit(&unit, &fixture).is_ok());
        let mut other = paths();
        other.state = "/another/state".into();
        assert!(validate_install_unit(&unit, &other).is_err());
        other = paths();
        other.config = "/another/config.json".into();
        assert!(validate_install_unit(&unit, &other).is_err());
        let unsafe_unit = unit
            .replace("KillMode=process", "KillMode=control-group")
            .replace(
                "<key>AbandonProcessGroup</key>\n  <true/>",
                "<key>AbandonProcessGroup</key>\n  <false/>",
            );
        assert!(validate_install_unit(&unsafe_unit, &fixture).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn launch_agent_ownership_accepts_any_spelling_of_this_binary_and_refuses_others() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/ownership-fixtures")
            .join(uuid::Uuid::new_v4().to_string());
        let bin = root.join("a&b/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let installed = bin.join("muqun-gateway");
        std::fs::write(&installed, b"binary").unwrap();
        let link = root.join("muqun-gateway");
        std::os::unix::fs::symlink(&installed, &link).unwrap();
        let other = root.join("other/muqun-gateway");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"another install").unwrap();

        let mut fixture = paths();
        fixture.exe = installed.clone();
        let plist = launch_agent_plist(&fixture);
        // The CLI run through a symlink, or relatively, is still this install.
        for exe in [
            installed.clone(),
            link.clone(),
            bin.join("../bin/muqun-gateway"),
        ] {
            validate_launch_agent_program(&plist, &exe).unwrap();
        }
        // A plist written through the symlink names the same binary too.
        fixture.exe = link.clone();
        validate_launch_agent_program(&launch_agent_plist(&fixture), &installed).unwrap();
        // A plist pointing at another installation is refused, naming it.
        fixture.exe = other.clone();
        let error = validate_launch_agent_program(&launch_agent_plist(&fixture), &installed)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&other.display().to_string()), "{error}");
        // What launchd has loaded is checked the same way.
        let print = |program: &Path| {
            format!(
                "gui/501/{SERVICE_LABEL} = {{\n\tstate = running\n\tprogram = {}\n\tpid = 7\n}}\n",
                program.display()
            )
        };
        validate_loaded_program(&print(&link), &installed).unwrap();
        assert!(validate_loaded_program(&print(&other), &installed).is_err());
        assert!(validate_loaded_program("\tstate = running\n", &installed).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn effective_systemd_identity_uses_exact_arguments_and_environment() {
        let fixture = paths();
        let exe = fixture.exe.to_str().unwrap();
        let valid = serde_json::json!({"type": "a{sv}", "data": [{
            "KillMode": {"type": "s", "data": "process"},
            "ExecStart": {"type": "a(sasbttttuii)", "data": [[exe, [exe, "run", "--config", fixture.config], false, 0, 0, 0, 0, 0, 0, 0]]},
            "Environment": {"type": "as", "data": [
                format!("HOME={}", fixture.home.display()),
                format!("MUQUN_GATEWAY_CONFIG_DIR={}", fixture.config.parent().unwrap().display()),
                format!("MUQUN_GATEWAY_STATE_DIR={}", fixture.state.display())
            ]},
            "EnvironmentFiles": {"type": "a(sb)", "data": []},
            "UnsetEnvironment": {"type": "as", "data": []},
            "ExecStop": {"type": "a(sasbttttuii)", "data": []},
            "ExecStopPost": {"type": "a(sasbttttuii)", "data": []}
        }]});
        assert!(validate_effective_unit(&valid, "process", &fixture, &[]).is_ok());
        for (name, replacement) in [
            ("KillMode", serde_json::json!("control-group")),
            (
                "EnvironmentFiles",
                serde_json::json!([["/foreign/environment", false]]),
            ),
            (
                "UnsetEnvironment",
                serde_json::json!(["MUQUN_GATEWAY_STATE_DIR"]),
            ),
            (
                "ExecStop",
                serde_json::json!([["/bin/kill", ["kill", "-1"], false]]),
            ),
            (
                "ExecStopPost",
                serde_json::json!([["/foreign/cleanup", [], false]]),
            ),
            (
                "ExecStart",
                serde_json::json!([[
                    exe,
                    [
                        exe,
                        "run",
                        "--config",
                        format!("{} extra", fixture.config.display())
                    ],
                    false
                ]]),
            ),
            // Same flattened command, different argv boundaries must not pass.
            (
                "ExecStart",
                serde_json::json!([[exe, [exe, "run --config", fixture.config], false]]),
            ),
            ("Environment", serde_json::json!(["HOME=/foreign"])),
        ] {
            let mut invalid = valid.clone();
            invalid["data"][0][name]["data"] = replacement;
            assert!(
                validate_effective_unit(&invalid, "process", &fixture, &[]).is_err(),
                "{name}"
            );
        }
        assert!(validate_effective_unit(&valid, "none", &fixture, &[]).is_err());
        assert!(validate_effective_unit(&serde_json::json!({}), "process", &fixture, &[]).is_err());

        let mut legacy_paths = paths();
        legacy_paths.config = fixture.home.join(".config/muqun-gateway/config.json");
        legacy_paths.state = fixture.home.join(".local/share/muqun-gateway");
        let mut legacy = valid.clone();
        legacy["data"][0]["Environment"]["data"] =
            serde_json::json!([format!("HOME={}", fixture.home.display())]);
        legacy["data"][0]["ExecStart"]["data"][0][1][3] = serde_json::json!(legacy_paths.config);
        assert!(validate_effective_unit(&legacy, "process", &legacy_paths, &[]).is_ok());
        for inherited in [
            "XDG_DATA_HOME=/foreign",
            "HERDR_PLUGIN_STATE_DIR=/foreign",
            "MUQUN_GATEWAY_CONFIG_DIR=/foreign",
        ] {
            assert!(validate_effective_unit(
                &legacy,
                "process",
                &legacy_paths,
                &[inherited.to_owned()]
            )
            .is_err());
            // Explicit service pins take precedence over inherited environment.
            assert!(
                validate_effective_unit(&valid, "process", &fixture, &[inherited.to_owned()])
                    .is_ok()
            );
        }
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
            for field in 0..6 {
                let mut paths = paths();
                let invalid = format!("/safe{control}ExecStart=/unwanted");
                match field {
                    0 => paths.exe = invalid.into(),
                    1 => paths.config = invalid.into(),
                    2 => paths.home = invalid.into(),
                    3 => paths.path = invalid,
                    4 => paths.lc_ctype = invalid,
                    _ => paths.state = invalid.into(),
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
