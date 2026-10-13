//! Real CLI processes with isolated config/state and fake supervisor commands.
//! No ambient agent service, terminal socket, or user service may enter a case.
#![cfg(unix)]

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::json;
use sha2::{Digest as _, Sha256};

struct Install {
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

impl Install {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("muqun-lifecycle-{}", uuid::Uuid::new_v4()));
        let config = root.join("config");
        let state = root.join("state");
        for dir in [&config, &state, &root.join("home"), &root.join("bin")] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let token = "isolated-lifecycle-admin";
        let token_hash =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(token.as_bytes()));
        fs::write(config.join("config.json"), serde_json::to_vec(&json!({
            "server_id": "isolated-lifecycle", "label": "test", "listen": address.to_string(),
            "public_url": format!("http://{address}"), "token_hash": token_hash,
            "sessions": [{"id": "qa", "label": "isolated", "backend": "tmux", "socket_path": root.join("no-terminal.sock")}],
            "autostart_backends": [], "opencode": {"enabled": false, "autostart": false},
            "deepseek": {"enabled": false}, "t3": {"enabled": false},
            "codex": {"enabled": false}, "agent_runtime": {"enabled": false}
        })).unwrap()).unwrap();
        fs::write(
            config.join("pairing.json"),
            serde_json::to_vec(&json!({"payload": {
                "kind": "muqun-gateway", "server_id": "isolated-lifecycle", "label": "test",
                "url": format!("http://{address}"), "token": token, "transport_key": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1_u8; 32])
            }}))
            .unwrap(),
        )
        .unwrap();
        for name in ["systemctl", "launchctl", "loginctl"] {
            let script = root.join("bin").join(name);
            fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$MOCK_LOG\"\ncase \"$*\" in *\"$MOCK_FAIL\"*) [ -z \"$MOCK_FAIL\" ] || { echo 'supervisor refused' >&2; exit 9; };; esac\ncase \"$*\" in *MainPID*) echo 1;; *KillMode*) cat \"$MOCK_POLICY\";; esac\nexit 0\n").unwrap();
            fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let script = root.join("bin/busctl");
        fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$MOCK_LOG\"\n[ -z \"$MOCK_BUS_FAIL\" ] || exit 1\ncase \"$*\" in *GetUnit*) printf '%s\\n' '{\"type\":\"o\",\"data\":[\"/org/freedesktop/systemd1/unit/fixture\"]}';; *GetAll*Manager*) cat \"$MOCK_MANAGER\";; *GetAll*) cat \"$MOCK_PROPERTIES\";; *) exit 1;; esac\n").unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
        // Even a backend liveness probe must never reach a host terminal.
        for name in [
            "tmux",
            "tailscale",
            "claude",
            "codex",
            "opencode",
            "t3",
            "deepseek",
        ] {
            let script = root.join("bin").join(name);
            fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            root,
            config,
            state,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_muqun-gateway"));
        command
            .args(args)
            .env_clear()
            .current_dir(&self.root)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.join("xdg-data"))
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("MUQUN_GATEWAY_CONFIG_DIR", &self.config)
            .env("MUQUN_GATEWAY_STATE_DIR", &self.state)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("LC_CTYPE", "C.UTF-8")
            .env("MOCK_LOG", self.root.join("commands.log"))
            .env("MOCK_FAIL", "")
            .env("MOCK_POLICY", self.root.join("effective-policy"))
            .env("MOCK_MANAGER", self.root.join("manager-properties.json"))
            .env(
                "MOCK_PROPERTIES",
                self.root.join("effective-properties.json"),
            )
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {} {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(self.state.join("gateway.log")).unwrap_or_default()
        );
        output
    }

    fn pid(&self) -> u32 {
        fs::read_to_string(self.state.join("gateway.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn unit(&self) -> PathBuf {
        let unit = self
            .root
            .join("home/.config/systemd/user/dev.osuki.muqun-gateway.service");
        fs::create_dir_all(unit.parent().unwrap()).unwrap();
        fs::write(&unit, format!("[Service]\nExecStart=\"{}\" run --config \"{}\"\nEnvironment=\"MUQUN_GATEWAY_STATE_DIR={}\"\nKillMode=process\n", env!("CARGO_BIN_EXE_muqun-gateway"), self.config.join("config.json").display(), self.state.display())).unwrap();
        let exe = env!("CARGO_BIN_EXE_muqun-gateway");
        self.effective_properties(&json!({"type": "a{sv}", "data": [{
            "KillMode": {"type": "s", "data": "process"},
            "ExecStart": {"type": "a(sasbttttuii)", "data": [[exe, [exe, "run", "--config", self.config.join("config.json")], false, 0, 0, 0, 0, 0, 0, 0]]},
            "Environment": {"type": "as", "data": [
                format!("HOME={}", self.root.join("home").display()),
                format!("MUQUN_GATEWAY_CONFIG_DIR={}", self.config.display()),
                format!("MUQUN_GATEWAY_STATE_DIR={}", self.state.display())
            ]},
            "EnvironmentFiles": {"type": "a(sb)", "data": []},
            "UnsetEnvironment": {"type": "as", "data": []},
            "ExecStop": {"type": "a(sasbttttuii)", "data": []},
            "ExecStopPost": {"type": "a(sasbttttuii)", "data": []}
        }]}));
        unit
    }

    #[cfg(target_os = "linux")]
    fn effective_properties(&self, reply: &serde_json::Value) {
        fs::write(
            self.root.join("effective-policy"),
            reply["data"][0]["KillMode"]["data"].as_str().unwrap(),
        )
        .unwrap();
        fs::write(
            self.root.join("effective-properties.json"),
            serde_json::to_vec(reply).unwrap(),
        )
        .unwrap();
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        // Kill only the exact child recorded in this fixture, never a port scan.
        if let Ok(text) = fs::read_to_string(self.state.join("gateway.pid")) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                // SAFETY: fixture PID belongs to the isolated spawned child.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A foreground Gateway in a private process group. Its synthetic Codex child
/// has a separate private group and is cleaned up through Gateway shutdown.
struct ForegroundGateway {
    child: std::process::Child,
    codex_pid: PathBuf,
    stubborn: bool,
    #[cfg(target_os = "linux")]
    codex_fd: Option<std::os::fd::OwnedFd>,
}

impl ForegroundGateway {
    fn capture_codex(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if self.codex_fd.is_some() || self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            let Some(pid) = fs::read_to_string(&self.codex_pid)
                .ok()
                .and_then(|pid| pid.parse::<u32>().ok())
            else {
                return;
            };
            let identity = || {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                let fields: Vec<_> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
                Some((
                    fields.get(1)?.parse::<u32>().ok()?,
                    fields.get(19)?.parse::<u64>().ok()?,
                ))
            };
            let Some(before) = identity().filter(|(parent, _)| *parent == self.child.id()) else {
                return;
            };
            // SAFETY: scalar syscall arguments; a successful result is a fresh fd.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            if fd >= 0 {
                // SAFETY: the fresh descriptor has not been adopted elsewhere.
                let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
                if identity() == Some(before) {
                    self.codex_fd = Some(fd);
                }
            }
        }
    }

    fn wait_listening(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !fs::read_to_string(self.codex_pid.with_file_name("foreground.out"))
            .unwrap_or_default()
            .contains("terminal gateway listening on")
        {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "fixture Gateway exited before listening"
            );
            assert!(
                Instant::now() < deadline,
                "fixture Gateway never started listening"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&mut self) {
        self.capture_codex();
        let started = Instant::now();
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        // Normal EOF must remain prompt. Only the deliberately hung fixture
        // consumes the 46s upstream grace plus bounded forced cleanup.
        let deadline = started + Duration::from_secs(if self.stubborn { 50 } else { 5 });
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "Gateway did not finish Codex shutdown"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        if self.stubborn {
            assert!(
                started.elapsed() >= Duration::from_secs(45),
                "stubborn cleanup was forced before upstream's watchdog window"
            );
        }
        #[cfg(target_os = "linux")]
        assert!(status.success());
        #[cfg(not(target_os = "linux"))]
        if self.stubborn {
            // Non-pidfd platforms deliberately refuse descendant signals and
            // report the unverified forced cleanup through the server result.
            assert!(!status.success());
            assert!(
                fs::read_to_string(self.codex_pid.with_file_name("foreground.err"))
                    .unwrap()
                    .contains("cleanup could not be verified")
            );
        } else {
            assert!(status.success());
        }
        eprintln!(
            "synthetic Codex shutdown stubborn={}: {:?}",
            self.stubborn,
            started.elapsed()
        );
    }
}

impl Drop for ForegroundGateway {
    fn drop(&mut self) {
        self.capture_codex();
        let group = self.child.id() as libc::pid_t;
        if self.child.try_wait().ok().flatten().is_none() {
            // Let the production ownership supervisor collect its separate
            // private child even when an assertion unwinds the fixture.
            unsafe {
                libc::kill(group, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(if self.stubborn { 50 } else { 6 });
            while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.codex_fd {
            // SAFETY: the descriptor targets the verified original fixture
            // incarnation even if its PID was reaped/reused while unwinding.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn configure_fake_codex(install: &Install, master_enabled: bool) -> PathBuf {
    let executable = install.root.join("bin/fake-codex");
    fs::write(&executable,r#"#!/bin/sh
[ "$1" = --fixture-ready ] && exit 0
printf '%s' "$$" > "$FAKE_CODEX_PID"
printf '%s\n' "$@" > "$FAKE_CODEX_ARGV"
while IFS= read -r line; do
    id=${line#*'"id":'}; id=${id%%,*}; id=${id%%\}*}
    case "$line" in
        *'"method":"initialize"'*) printf '{"id":%s,"result":{"userAgent":"codex_cli_rs/0.162.1 (synthetic fixture)","codexHome":"/synthetic/codex-home","platformFamily":"unix","platformOs":"synthetic"}}\n' "$id" ;;
        *'"method":"account/read"'*) printf '{"id":%s,"result":{"account":null,"requiresOpenaiAuth":false}}\n' "$id"; printf 'ready' > "$FAKE_CODEX_READY" ;;
        *'"method":"model/list"'*) printf '{"id":%s,"result":{"data":[],"nextCursor":null}}\n' "$id" ;;
        *'"method":"config/read"'*) printf '{"id":%s,"result":{"config":{"model_provider":null}}}\n' "$id" ;;
    esac
done
if [ "$FAKE_CODEX_STUBBORN" = true ]; then while :; do :; done; fi
"#).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    // Side-effect-free readiness addresses only a fake immutable script's
    // pre-exec ETXTBSY; it is not a retry of upstream agent work.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Command::new(&executable).arg("--fixture-ready").status() {
            Ok(status) => {
                assert!(status.success());
                break;
            }
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                assert!(
                    Instant::now() < deadline,
                    "fake Codex fixture remained busy"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fake Codex readiness failed: {error}"),
        }
    }
    let path = install.config.join("config.json");
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent_runtime"] = json!({"enabled":master_enabled});
    config["codex"] = json!({"binary":executable});
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    executable
}

fn foreground_gateway(install: &Install, stubborn: bool) -> ForegroundGateway {
    use std::os::unix::process::CommandExt as _;
    let codex_pid = install.root.join("fake-codex.pid");
    let mut command = install.command(&["run"]);
    command
        .process_group(0)
        .env("FAKE_CODEX_PID", &codex_pid)
        .env("FAKE_CODEX_ARGV", install.root.join("fake-codex.argv"))
        .env("FAKE_CODEX_READY", install.root.join("fake-codex.ready"))
        .env(
            "FAKE_CODEX_STUBBORN",
            if stubborn { "true" } else { "false" },
        )
        .stdout(fs::File::create(install.root.join("foreground.out")).unwrap())
        .stderr(fs::File::create(install.root.join("foreground.err")).unwrap());
    ForegroundGateway {
        child: command.spawn().unwrap(),
        codex_pid,
        stubborn,
        #[cfg(target_os = "linux")]
        codex_fd: None,
    }
}

#[test]
fn codex_private_fake_child_is_started_from_config_and_reaped_on_sigterm() {
    for stubborn in [false, true] {
        let install = Install::new();
        let executable = configure_fake_codex(&install, true);
        if stubborn {
            // Both omitted blocks default on, and normal PATH selects only
            // this immutable fixture, never an ambient installed Codex.
            let path_binary = install.root.join("bin/codex");
            fs::remove_file(&path_binary).unwrap();
            fs::hard_link(&executable, &path_binary).unwrap();
            let path = install.config.join("config.json");
            let mut config: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            config.as_object_mut().unwrap().remove("codex");
            config.as_object_mut().unwrap().remove("agent_runtime");
            fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        }
        let mut gateway = foreground_gateway(&install, stubborn);
        let deadline = Instant::now() + Duration::from_secs(8);
        while !install.root.join("fake-codex.ready").exists() {
            gateway.capture_codex();
            assert!(
                gateway.child.try_wait().unwrap().is_none(),
                "Gateway exited before fake Codex readiness"
            );
            assert!(
                Instant::now() < deadline,
                "fake Codex was not initialized: {}",
                fs::read_to_string(install.root.join("foreground.err")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let codex_pid: libc::pid_t = fs::read_to_string(&gateway.codex_pid)
            .unwrap()
            .parse()
            .unwrap();
        gateway.capture_codex();
        assert_eq!(
            fs::read_to_string(install.root.join("fake-codex.argv")).unwrap(),
            "app-server\n--listen\nstdio://\n"
        );
        assert_eq!(unsafe { libc::getpgid(codex_pid) }, codex_pid);
        assert_ne!(codex_pid, gateway.child.id() as libc::pid_t);
        gateway.stop();
        assert_eq!(
            unsafe { libc::kill(codex_pid, 0) },
            -1,
            "private fake child survived Gateway shutdown"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}

#[test]
fn codex_master_off_cli_and_bind_failure_never_start_the_configured_fake_child() {
    let install = Install::new();
    configure_fake_codex(&install, false);
    let list = install.ok(&["agent", "--json"]);
    let agents: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert!(agents
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a["status"] == "disabled"));
    assert!(!install.root.join("fake-codex.pid").exists());
    assert!(!install.root.join("fake-codex.argv").exists());
    configure_fake_codex(&install, true);
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let path = install.config.join("config.json");
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["listen"] = json!(occupied.local_addr().unwrap().to_string());
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    let result = install.run(&["run"]);
    assert!(!result.status.success());
    assert!(!install.root.join("fake-codex.pid").exists());
    assert!(!install.root.join("fake-codex.argv").exists());
}

#[test]
fn codex_explicit_disable_and_master_off_reach_the_running_server_without_child_startup() {
    for master_off in [true, false] {
        let install = Install::new();
        configure_fake_codex(&install, !master_off);
        if !master_off {
            let path = install.config.join("config.json");
            let mut config: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            config["codex"]["enabled"] = json!(false);
            fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        }
        let mut gateway = foreground_gateway(&install, false);
        gateway.wait_listening();
        gateway.stop();
        assert!(!gateway.codex_pid.exists());
        assert!(!install.root.join("fake-codex.argv").exists());
    }
}

#[test]
fn ordinary_lifecycle_is_idempotent_preserves_pairing_and_ignores_stale_pid() {
    let install = Install::new();
    let identity = fs::read(install.config.join("pairing.json")).unwrap();
    let mut task = Command::new("/bin/sleep").arg("60").spawn().unwrap();
    fs::write(install.state.join("gateway.pid"), task.id().to_string()).unwrap();
    install.ok(&["stop"]);
    assert!(
        task.try_wait().unwrap().is_none(),
        "a stale PID must not signal an unrelated task"
    );
    task.kill().unwrap();
    task.wait().unwrap();
    install.ok(&["start"]);
    let first = install.pid();
    install.ok(&["start"]);
    assert_eq!(install.pid(), first);
    let status = install.ok(&["status"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains(&format!("running pid {first}")));
    install.ok(&["restart"]);
    assert_ne!(install.pid(), first);
    install.ok(&["stop"]);
    install.ok(&["stop"]);
    assert!(String::from_utf8_lossy(&install.ok(&["status"]).stdout).contains("gateway: stopped"));
    assert_eq!(
        fs::read(install.config.join("pairing.json")).unwrap(),
        identity
    );
}

#[test]
fn a_startup_bind_failure_is_reported_and_leaves_no_detached_child() {
    let install = Install::new();
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(install.config.join("config.json")).unwrap()).unwrap();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config["listen"] = occupied.local_addr().unwrap().to_string().into();
    fs::write(
        install.config.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let output = install.run(&["start"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("exited during startup"), "{stderr}");
    // The real cause, naming the holder -- this test process -- rather than
    // mistaking the exiting gateway for an unrecognized lock owner.
    assert!(stderr.contains("is already in use by"), "{stderr}");
    assert!(
        stderr.contains(&format!("(pid {})", std::process::id())),
        "{stderr}"
    );
    assert!(!stderr.contains("unrecognized process"), "{stderr}");
    assert!(!install.state.join("gateway.pid").exists());
}

#[test]
fn concurrent_starts_spawn_only_one_gateway() {
    let install = Install::new();
    let first = install
        .command(&["start"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let second = install
        .command(&["start"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    assert!(outputs.iter().any(|output| output.status.success()));
    for output in &outputs {
        if !output.status.success() {
            assert!(String::from_utf8_lossy(&output.stderr).contains("another lifecycle command"));
        }
    }
    let pid = install.pid();
    install.ok(&["start"]);
    assert_eq!(install.pid(), pid);
    let log = fs::read_to_string(install.state.join("gateway.log")).unwrap();
    assert_eq!(log.matches("terminal gateway listening").count(), 1);
    install.ok(&["stop"]);
}

#[test]
fn an_unrecognized_lock_owner_refuses_restart_without_spawning_or_signalling() {
    let install = Install::new();
    let mut lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(install.state.join("gateway.lock"))
        .unwrap();
    // SAFETY: lock is owned and open; this holds only a fixture directory.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    write!(lock, "{}", std::process::id()).unwrap();
    lock.flush().unwrap();
    let output = install.run(&["restart"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized process"));
    assert!(!install.state.join("gateway.pid").exists());
    assert!(!install.state.join("gateway.log").exists());
}

#[test]
fn a_pid_recording_failure_stops_the_child_and_reports_the_failure() {
    let install = Install::new();
    fs::create_dir(install.state.join("gateway.pid")).unwrap();
    let output = install.run(&["start"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("child was stopped because its PID could not be recorded"));
    assert!(install.state.join("gateway.pid").is_dir());
}

#[test]
fn service_install_refuses_an_inconsistent_identity_before_registering() {
    let install = Install::new();
    fs::write(install.config.join("pairing.json"), "{}").unwrap();
    let output = install.run(&["service", "install"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("consistent gateway identity"));
    assert!(!install.root.join("commands.log").exists());
    assert!(!install.state.join("gateway.pid").exists());
}

#[test]
fn legacy_plugin_environment_is_retained_until_the_import_marker_redirects_it() {
    let install = Install::new();
    let legacy_command = |args: &[&str]| {
        let mut command = install.command(args);
        command
            .env_remove("MUQUN_GATEWAY_CONFIG_DIR")
            .env_remove("MUQUN_GATEWAY_STATE_DIR")
            .env("HERDR_PLUGIN_CONFIG_DIR", &install.config)
            .env("HERDR_PLUGIN_STATE_DIR", &install.state);
        command
    };
    for args in [&["start"][..], &["stop"][..]] {
        let output = legacy_command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(target_os = "macos")]
    let standalone_config = install
        .root
        .join("home/Library/Application Support/muqun-gateway");
    #[cfg(not(target_os = "macos"))]
    let standalone_config = install.root.join("xdg-config/muqun-gateway");
    #[cfg(target_os = "macos")]
    let standalone_state = standalone_config.clone();
    #[cfg(not(target_os = "macos"))]
    let standalone_state = install.root.join("xdg-data/muqun-gateway");
    fs::create_dir_all(&standalone_config).unwrap();
    for name in ["config.json", "pairing.json"] {
        fs::copy(install.config.join(name), standalone_config.join(name)).unwrap();
    }
    fs::write(
        standalone_config.join(".herdr-plugin-imported"),
        "fixture import",
    )
    .unwrap();
    let output = legacy_command(&["start"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pid = fs::read_to_string(standalone_state.join("gateway.pid")).unwrap();
    // Ensure fixture cleanup can still signal exactly this child after an assertion failure.
    fs::write(install.state.join("gateway.pid"), &pid).unwrap();
    let output = legacy_command(&["stop"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_file(install.state.join("gateway.pid")).unwrap();
    assert!(!standalone_state.join("gateway.pid").exists());
    assert_eq!(
        fs::read(install.config.join("pairing.json")).unwrap(),
        fs::read(standalone_config.join("pairing.json")).unwrap()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn service_actions_use_supervisor_and_fail_without_removing_registration() {
    let install = Install::new();
    let unit = install.unit();
    install.ok(&["stop"]);
    let calls = fs::read_to_string(install.root.join("commands.log")).unwrap();
    assert!(calls.contains("--user stop dev.osuki.muqun-gateway.service"));
    for action in ["start", "stop", "restart"] {
        let output = install
            .command(&[action])
            .env("MOCK_FAIL", action)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("supervisor refused"));
        assert!(unit.exists());
        assert!(!install.state.join("gateway.pid").exists());
    }
    let output = install
        .command(&["service", "uninstall"])
        .env("MOCK_FAIL", "disable")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        unit.exists(),
        "failed disable must retain recovery information"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn lifecycle_refuses_a_foreign_or_task_killing_service_unit() {
    let install = Install::new();
    let unit = install.unit();
    let original = fs::read_to_string(&unit).unwrap();
    let original_properties: serde_json::Value =
        serde_json::from_slice(&fs::read(install.root.join("effective-properties.json")).unwrap())
            .unwrap();
    for field in ["KillMode", "config", "state", "exe", "config-environment"] {
        let mut reply = original_properties.clone();
        let properties = &mut reply["data"][0];
        match field {
            "KillMode" => properties["KillMode"]["data"] = "control-group".into(),
            "config" => properties["ExecStart"]["data"][0][1][3] = "/foreign/config.json".into(),
            "state" => {
                properties["Environment"]["data"][2] =
                    "MUQUN_GATEWAY_STATE_DIR=/foreign/state".into()
            }
            "exe" => properties["ExecStart"]["data"][0][0] = "/foreign/gateway".into(),
            _ => {
                properties["Environment"]["data"][1] =
                    "MUQUN_GATEWAY_CONFIG_DIR=/foreign/config".into()
            }
        }
        install.effective_properties(&reply);
        // Safe base file remains unchanged: only the effective drop-in values differ.
        for args in [
            &["stop"][..],
            &["restart"][..],
            &["service", "uninstall"][..],
        ] {
            let output = install.run(args);
            assert!(!output.status.success(), "{field} {args:?}");
            assert_eq!(fs::read_to_string(&unit).unwrap(), original);
            let calls = fs::read_to_string(install.root.join("commands.log")).unwrap();
            for verb in ["stop", "restart", "disable"] {
                assert!(!calls.contains(&format!("--user {verb}")), "{calls}");
            }
            assert!(!install.state.join("gateway.pid").exists());
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn effective_service_inspection_fails_closed_and_checks_legacy_inheritance() {
    let install = Install::new();
    let unit = install.unit();
    let original = fs::read(&unit).unwrap();
    let mut reply: serde_json::Value =
        serde_json::from_slice(&fs::read(install.root.join("effective-properties.json")).unwrap())
            .unwrap();
    let inherited = reply["data"][0]["Environment"].clone();
    reply["data"][0]["Environment"]["data"] =
        serde_json::json!([format!("HOME={}", install.root.join("home").display())]);
    install.effective_properties(&reply);
    fs::write(
        install.root.join("manager-properties.json"),
        serde_json::to_vec(&json!({"type": "a{sv}", "data": [{"Environment": inherited}]}))
            .unwrap(),
    )
    .unwrap();
    install.ok(&["stop"]);
    fs::write(install.root.join("commands.log"), "").unwrap();
    fs::write(install.root.join("manager-properties.json"), serde_json::to_vec(&json!({"type": "a{sv}", "data": [{"Environment": {"type": "as", "data": ["MUQUN_GATEWAY_STATE_DIR=/foreign"]}}]})).unwrap()).unwrap();
    for args in [
        &["stop"][..],
        &["restart"][..],
        &["service", "uninstall"][..],
    ] {
        let output = install.run(args);
        assert!(!output.status.success());
        let output = install
            .command(args)
            .env("MOCK_BUS_FAIL", "yes")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read(&unit).unwrap(), original);
    }
    let output = install
        .command(&["stop"])
        .env("MOCK_FAIL", "daemon-reload")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let calls = fs::read_to_string(install.root.join("commands.log")).unwrap();
    for verb in ["stop", "restart", "disable"] {
        assert!(!calls.contains(&format!("--user {verb}")), "{calls}");
    }
    assert!(!install.state.join("gateway.pid").exists());
}

struct ManagerPty {
    master: fs::File,
    child: std::process::Child,
}

impl ManagerPty {
    fn open(install: &Install) -> Self {
        let mut master = -1;
        let mut slave = -1;
        // macOS declares the termios and winsize arguments `*mut`, Linux
        // `*const`; mutable pointers coerce to either.
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty fills two fresh descriptors and reads the winsize.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::addr_of_mut!(size),
                )
            },
            0
        );
        // SAFETY: each fresh descriptor is transferred to exactly one File.
        let master = unsafe { fs::File::from_raw_fd(master) };
        let slave = unsafe { fs::File::from_raw_fd(slave) };
        let mut command = install.command(&["manage"]);
        command
            .env("TERM", "xterm-256color")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        use std::os::unix::process::CommandExt as _;
        // SAFETY: only async-signal-safe session/tty syscalls run before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self { master, child }
    }

    fn send(&mut self, keys: &[u8]) {
        self.master.write_all(keys).unwrap();
        self.master.flush().unwrap();
    }

    fn until(&mut self, text: &str) -> String {
        self.until_all(&[text])
    }

    /// Wait for every one of `texts`: a frame arrives in reads of any size,
    /// so the first of two texts on one row can be read before the second.
    fn until_all(&mut self, texts: &[&str]) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut bytes = Vec::new();
        loop {
            let output = String::from_utf8_lossy(&bytes);
            if texts.iter().all(|text| output.contains(text)) {
                return output.into_owned();
            }
            assert!(
                Instant::now() < deadline,
                "manager did not render {texts:?}: {output}"
            );
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll points to one valid descriptor entry.
            if unsafe { libc::poll(&mut poll, 1, 100) } > 0 {
                let mut buffer = [0_u8; 8192];
                let size = self.master.read(&mut buffer).unwrap();
                assert!(size > 0, "manager closed before rendering {texts:?}");
                bytes.extend_from_slice(&buffer[..size]);
            }
        }
    }
}

impl Drop for ManagerPty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn manager_pty_navigates_scrolls_resizes_and_keeps_start_failures_in_the_ui() {
    let install = Install::new();
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(install.config.join("config.json")).unwrap()).unwrap();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config["listen"] = occupied.local_addr().unwrap().to_string().into();
    config["public_url"] = format!("http://{}", occupied.local_addr().unwrap()).into();
    let mut sessions = Vec::new();
    for index in 0..40 {
        let mut session = config["sessions"][0].clone();
        session["id"] = format!("qa-{index:02}").into();
        session["label"] = format!("终端 e\u{301} {index:02}").into();
        sessions.push(session);
    }
    config["sessions"] = sessions.into();
    fs::write(
        install.config.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let mut manager = ManagerPty::open(&install);
    let output = manager.until("s start · t stop · r restart");
    assert!(output.contains("Runtime"));
    assert!(output.contains("Connectivity"));
    assert!(
        output.contains("\x1b[7m"),
        "active tab is not reversed: {output:?}"
    );
    manager.send(b"2\x1b[F");
    let output = manager.until_all(&["▌ qa-39", "终端 e\u{301} 39"]);
    assert!(
        output.contains("终端 e\u{301} 39"),
        "wide selected label missing: {output:?}"
    );
    // Enter opens the backend's actions; its first is "Make default".
    manager.send(b"\r");
    manager.until("Make default");
    manager.send(b"\r");
    manager.until("default is now qa-39");
    let changed: serde_json::Value =
        serde_json::from_slice(&fs::read(install.config.join("config.json")).unwrap()).unwrap();
    assert_eq!(changed["sessions"][0]["id"], "qa-39");
    manager.send(b"\r");
    manager.until("Remove backend");
    manager.send(b"\x1b[B\x1b[B\r");
    manager.until("Remove terminal backend?");
    manager.send(b"\x1b");
    manager.until("backend unchanged");
    manager.send(b"3");
    manager.until("No devices paired yet.");
    manager.send(b"\r");
    manager.until("1 Scan the QR in Muqun");
    let size = libc::winsize {
        ws_row: 8,
        ws_col: 40,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: this ioctl resizes only this fixture's PTY.
    assert_eq!(
        unsafe { libc::ioctl(manager.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) },
        0
    );
    manager.until("Resize for the full QR");
    let size = libc::winsize {
        ws_row: 40,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(manager.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) },
        0
    );
    // Confirm the larger frame before sending a standalone Esc: resize
    // notification and key decoding are asynchronous, separate transitions.
    manager.until("1 Scan the QR in Muqun");
    manager.send(b"\x1b");
    manager.until("No devices paired yet.");
    manager.send(b"4\r");
    manager.until("Turn Gateway login autostart on?");
    // Default confirmation is cancel: no remembered shortcut and no registration.
    manager.send(b"\r");
    manager.until("gateway autostart unchanged");
    manager.send(b"\x1b[B\r");
    manager.until("Edit Gateway address");
    manager.send(b"\x1b");
    manager.until("url unchanged");
    // Detect address offers the address; Esc leaves config.json as it was.
    let before = fs::read(install.config.join("config.json")).unwrap();
    manager.send(b"\x1b[B\r");
    manager.until("Edit Gateway address");
    manager.send(b"\x1b");
    manager.until("url unchanged");
    assert_eq!(
        fs::read(install.config.join("config.json")).unwrap(),
        before
    );
    // History checkpoints: the chooser writes the one key it changes.
    manager.send(b"\x1b[B\x1b[B\x1b[B\r");
    manager.until("only while Gateway runs");
    manager.send(b"\x1b[B\r");
    manager.until("History checkpoints: SQLite");
    let mut after: serde_json::Value =
        serde_json::from_slice(&fs::read(install.config.join("config.json")).unwrap()).unwrap();
    assert_eq!(after["history"]["storage"], "sqlite");
    after.as_object_mut().unwrap().remove("history");
    let mut before: serde_json::Value = serde_json::from_slice(&before).unwrap();
    before.as_object_mut().unwrap().remove("history");
    assert_eq!(after, before, "only history.storage changed");
    // Enter is not the start key on Overview; `s` is.
    manager.send(b"1\r");
    manager.until("Status: ready");
    manager.send(b"s");
    manager.until("Action failed: gateway exited during startup");
    assert!(
        manager.child.try_wait().unwrap().is_none(),
        "failed start exited the manager"
    );
    assert!(!install.state.join("gateway.pid").exists());
    manager.send(b"q");
    manager.until("\x1b[?1049l");
    assert!(manager.child.wait().unwrap().success());
    // SAFETY: master remains owned and termios is writable storage.
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(
        unsafe { libc::tcgetattr(manager.master.as_raw_fd(), &mut termios) },
        0
    );
    assert_ne!(termios.c_lflag & libc::ICANON, 0);
    assert_ne!(termios.c_lflag & libc::ECHO, 0);
}

#[test]
fn manager_enter_targets_the_selected_device_and_esc_only_cancels_revocation() {
    let install = Install::new();
    let file = install.state.join("devices.json");
    let records = serde_json::to_vec(&json!([
        {"id": "first", "name": "First phone 中文 e\u{301}", "token_hash": "fixture-first", "paired_unix_ms": 1},
        {"id": "second", "name": "Second phone", "token_hash": "fixture-second", "paired_unix_ms": 2}
    ])).unwrap();
    fs::write(&file, &records).unwrap();
    let mut manager = ManagerPty::open(&install);
    manager.until("s start · t stop · r restart");
    // Enter shows the device; x there reviews its revocation.
    manager.send(b"3\x1b[B\r");
    let output = manager.until("x revoke · Esc back");
    assert!(output.contains("First phone 中文 e\u{301}"));
    manager.send(b"x");
    let output = manager.until("Enter confirm");
    assert!(output.contains("Selected: First phone 中文 e\u{301}"));
    assert!(output.contains("loses Gateway access immediately"));
    manager.send(b"\x1b");
    manager.until("revoke cancelled");
    assert!(manager.child.try_wait().unwrap().is_none());
    assert_eq!(fs::read(&file).unwrap(), records);
    manager.send(b"q");
    manager.until("\x1b[?1049l");
    assert!(manager.child.wait().unwrap().success());
}

#[test]
fn manager_compact_pairing_keeps_the_full_address_reachable_without_resizing() {
    let install = Install::new();
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(install.config.join("config.json")).unwrap()).unwrap();
    let address = "https://isolated-gateway.example.test/a-long-address-for-pairing/final-entry";
    config["public_url"] = address.into();
    fs::write(
        install.config.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let mut manager = ManagerPty::open(&install);
    manager.until("s start · t stop · r restart");
    let size = libc::winsize {
        ws_row: 8,
        ws_col: 40,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: resize only this fixture's owned PTY.
    assert_eq!(
        unsafe { libc::ioctl(manager.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) },
        0
    );
    // Wait for the resized Overview frame before starting pairing, so PTY
    // resize delivery cannot overlap the navigation input being asserted.
    manager.until("Tab next view · q quit");
    manager.send(b"3\r");
    let output = manager.until(&address[..30]);
    assert!(output.contains("Resize for the full QR"));
    // Address has a nine-cell prefix, leaving thirty cells per wrapped segment.
    for segment in address.as_bytes().chunks(30).skip(1) {
        manager.send(b"\x1b[B");
        manager.until(std::str::from_utf8(segment).unwrap());
    }
    assert!(manager.child.try_wait().unwrap().is_none());
    manager.send(b"q");
    manager.until("\x1b[?1049l");
    assert!(manager.child.wait().unwrap().success());
}
