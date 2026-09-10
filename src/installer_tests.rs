//! Run the real installer against fake binaries, never the host's services.
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn install_case(herdr: bool, tmux: bool, answer: Option<&str>, installed: bool) -> String {
    install_case_with_options(herdr, tmux, answer, installed, None, None)
}

fn install_case_with_options(
    herdr: bool,
    tmux: bool,
    answer: Option<&str>,
    installed: bool,
    port: Option<&str>,
    socket_name: Option<&str>,
) -> String {
    let root = std::env::temp_dir().join(format!("muqun-installer-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let bin = root.join("bin");
    let home = root.join("home");
    let install = root.join("install");
    for path in [
        &bin,
        &home,
        &install,
        &root.join("config"),
        &root.join("data"),
        &root.join("state"),
        &root.join("cache"),
        &root.join("runtime"),
        &root.join("tmp"),
    ] {
        fs::create_dir(path).unwrap();
        assert!(fs::canonicalize(path).unwrap().starts_with(&root));
    }
    // A closed PATH makes backend detection independent of the test host.
    for name in [
        "awk", "cut", "head", "chmod", "mv", "mkdir", "basename", "id", "uname", "cp",
    ] {
        let program = [format!("/usr/bin/{name}"), format!("/bin/{name}")]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .unwrap();
        std::os::unix::fs::symlink(program, bin.join(name)).unwrap();
    }
    if herdr {
        executable(&bin.join("herdr"), "#!/bin/sh\necho 'herdr 0.9.0'\n");
    }
    if tmux {
        executable(&bin.join("tmux"), "#!/bin/sh\nexit 0\n");
    }
    for name in ["systemctl", "launchctl", "loginctl"] {
        executable(
            &bin.join(name),
            &format!("#!/bin/sh\nprintf '{name} %s\\n' \"$*\" >> \"$MOCK_LOG\"\n"),
        );
    }
    // Refuse an unexpected destination BEFORE copying. A broken installer
    // override must fail the fixture, never replace a developer's binary.
    executable(&bin.join("curl"), "#!/bin/sh\nwhile [ $# -gt 0 ]; do\nif [ \"$1\" = -o ]; then\n[ \"$2\" = \"$MOCK_INSTALL_DIR/muqun-gateway.new\" ] || exit 91\ncp \"$MOCK_BINARY\" \"$2\"; exit\nfi\nshift\ndone\nexit 1\n");
    let fake = root.join("gateway");
    executable(
        &fake,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$MOCK_LOG"
if [ "${1:-}" = setup ]; then
  printf '%s\000' "$@" > "$MOCK_SETUP_LOG"
fi
case "$*" in
'service status') echo "service: $MOCK_SERVICE";;
'backend list') printf '%b' "$MOCK_BACKENDS";;
esac
"#,
    );
    let log = root.join("calls.log");
    let setup_log = root.join("setup.args");
    let socket = socket_name.map(|name| root.join(name));
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let mut cmd = if answer.is_some() {
        let mut cmd = Command::new("/usr/bin/script");
        #[cfg(target_os = "macos")]
        cmd.args(["-q", "/dev/null", "/bin/sh"]).arg(&installer);
        #[cfg(not(target_os = "macos"))]
        cmd.args(["-q", "-c"])
            .arg(format!("/bin/sh '{}'", installer.display()))
            .arg("/dev/null");
        cmd
    } else {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(&installer);
        // No controlling terminal: a piped install cannot silently say yes.
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd
    };
    let backends = format!(
        "{}{}",
        if tmux {
            "tmux\\ttmux\\tShell\\tdefault\\n"
        } else {
            ""
        },
        if herdr {
            "herdr\\therdr\\tHerdr\\tdefault\\n"
        } else {
            ""
        }
    );
    // No ambient installer override, shell startup file, backend context,
    // proxy, or service-manager address may enter this subprocess.
    cmd.env_clear()
        .current_dir(&root)
        .env("PATH", &bin)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root.join("tmp"))
        .env("LC_ALL", "C")
        .env("TERM", "dumb")
        .env("MUQUN_GATEWAY_INSTALL_DIR", &install)
        .env("MOCK_INSTALL_DIR", &install)
        .env("MOCK_BINARY", &fake)
        .env("MOCK_LOG", &log)
        .env("MOCK_SETUP_LOG", &setup_log)
        .env("MOCK_BACKENDS", backends)
        .env(
            "MOCK_SERVICE",
            if installed {
                "installed"
            } else {
                "not installed"
            },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(port) = port {
        cmd.env("MUQUN_GATEWAY_PORT", port);
    }
    if let Some(socket) = &socket {
        assert!(socket.starts_with(&root));
        cmd.env("MUQUN_GATEWAY_TMUX_SOCKET", socket);
    }
    let mut child = cmd.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = [0; 1024];
        while let Ok(count) = output.read(&mut bytes) {
            if count == 0
                || tx
                    .send(String::from_utf8_lossy(&bytes[..count]).into_owned())
                    .is_err()
            {
                break;
            }
        }
    });
    let mut transcript = String::new();
    let mut backend_answered = false;
    let mut service_answered = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        for text in rx.try_iter() {
            transcript.push_str(&text);
        }
        if let Some(answer) = answer {
            if !backend_answered && transcript.contains("Enable backend startup? [y/N]") {
                input.write_all(format!("{answer}\n").as_bytes()).unwrap();
                backend_answered = true;
            }
            if !service_answered && transcript.contains("Set it up now? [Y/n]") {
                input.write_all(b"n\n").unwrap();
                service_answered = true;
            }
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!(
                "installer fixture timed out: {}\n{transcript}",
                root.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(input);
    reader.join().unwrap();
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(status.success(), "installer failed: {calls}");
    let installed_binary = fs::canonicalize(install.join("muqun-gateway")).unwrap();
    assert_eq!(installed_binary.parent(), Some(install.as_path()));
    assert!(installed_binary.starts_with(&root));
    assert_eq!(fs::read(installed_binary).unwrap(), fs::read(fake).unwrap());
    // Also detect a regression that silently falls back to HOME instead of
    // honoring the explicit install override (HOME itself is isolated).
    assert!(!home.join(".local/bin/muqun-gateway").exists());
    // Unlike the human-readable call log, NUL boundaries prove argv remains
    // intact when a path contains whitespace or shell metacharacters.
    let arguments = fs::read(&setup_log).unwrap();
    let mut expected = vec!["setup", "--backend", if tmux { "tmux" } else { "herdr" }];
    if let Some(port) = port {
        expected.extend(["--port", port]);
    }
    if let Some(socket) = &socket {
        if tmux {
            expected.extend(["--socket-path", socket.to_str().unwrap()]);
        }
    }
    let expected: Vec<u8> = expected
        .iter()
        .flat_map(|argument| argument.bytes().chain(std::iter::once(0)))
        .collect();
    // All contents are generated fixtures under this unique test-owned path.
    fs::remove_dir_all(root).unwrap();
    assert_eq!(
        arguments, expected,
        "installer changed setup argument boundaries"
    );
    calls
}

#[test]
fn installer_preserves_custom_socket_and_port_argument_boundaries() {
    for (herdr, tmux) in [(false, true), (true, true), (true, false)] {
        install_case_with_options(
            herdr,
            tmux,
            None,
            false,
            Some("24861"),
            Some("socket directory/quoted 'name' [work]*.sock"),
        );
    }
    install_case_with_options(
        false,
        true,
        None,
        false,
        None,
        Some("socket directory/default.sock"),
    );
}

#[test]
fn installer_requires_explicit_consent_and_supports_each_backend_combination() {
    for (herdr, tmux) in [(false, true), (true, false), (true, true)] {
        let calls = install_case(herdr, tmux, Some("y"), false);
        assert_eq!(
            calls.contains("backend autostart herdr on"),
            herdr,
            "{calls}"
        );
        assert_eq!(calls.contains("backend autostart tmux on"), tmux, "{calls}");
        assert!(!calls.contains("service install"));
    }
    for answer in [None, Some(""), Some("n"), Some("invalid")] {
        let calls = install_case(true, true, answer, false);
        assert!(!calls.contains("backend autostart"));
        assert!(!calls.contains("service install"));
    }
}

#[test]
fn installer_refreshes_an_existing_service_without_stopping_it_by_pid() {
    let calls = install_case(true, true, Some("n"), true);
    assert!(calls.contains("service install"));
    assert!(!calls.lines().any(|line| line == "stop" || line == "start"));
    assert!(!calls.contains("backend autostart"));
}
