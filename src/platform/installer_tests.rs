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

/// A real native header, not mocked file(1) output or a shebang pretending to
/// be a release image. The small fixture execs the isolated shell command fake.
fn native_fixture() -> &'static [u8] {
    static IMAGE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    IMAGE.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("muqun-native-fixture-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let source = root.join("fixture.rs");
        let binary = root.join("fixture");
        fs::write(
            &source,
            r#"use std::os::unix::process::CommandExt;
fn main() {
    let error = std::process::Command::new(std::env::var_os("MOCK_SCRIPT").unwrap())
        .args(std::env::args_os().skip(1)).exec();
    panic!("fixture exec failed: {error}");
}"#,
        )
        .unwrap();
        let output = Command::new("rustc")
            .args(["--edition", "2021"])
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let image = fs::read(binary).unwrap();
        fs::remove_dir_all(root).unwrap();
        image
    })
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
    match run_installer(herdr, tmux, answer, installed, port, socket_name, "running") {
        Ok(transcript) => transcript,
        Err(calls) => panic!("installer failed: {calls}"),
    }
}

/// `Err` carries the call log when the installer exits non-zero.
#[allow(clippy::too_many_arguments)]
fn run_installer(
    herdr: bool,
    tmux: bool,
    answer: Option<&str>,
    installed: bool,
    port: Option<&str>,
    socket_name: Option<&str>,
    gateway: &str,
) -> Result<String, String> {
    run_installer_case(
        herdr,
        tmux,
        answer,
        installed,
        port,
        socket_name,
        gateway,
        DownloadCase::default(),
    )
}

#[derive(Default)]
struct DownloadCase {
    existing: Option<&'static str>,
    fault: Option<&'static str>,
    tag: Option<&'static str>,
    shasum: bool,
    term: Option<&'static str>,
    no_color: bool,
    transcript: bool,
    darwin: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_installer_case(
    herdr: bool,
    tmux: bool,
    answer: Option<&str>,
    installed: bool,
    port: Option<&str>,
    socket_name: Option<&str>,
    gateway: &str,
    case: DownloadCase,
) -> Result<String, String> {
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
        "awk", "cut", "head", "chmod", "mkdir", "basename", "id", "uname", "cp", "sleep", "grep",
        "mktemp", "rm", "wc", "ln", "cat", "ls", "stty", "od", "perl", "stat",
    ] {
        let program = [format!("/usr/bin/{name}"), format!("/bin/{name}")]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .unwrap();
        std::os::unix::fs::symlink(program, bin.join(name)).unwrap();
    }
    if case.darwin {
        fs::remove_file(bin.join("uname")).unwrap();
        executable(
            &bin.join("uname"),
            "#!/bin/sh\ncase \"$1\" in -s) echo Darwin;; -m) echo x86_64;; *) exit 1;; esac\n",
        );
        // Only the emulated Darwin branch uses a synthetic Mach-O header;
        // ordinary cases inspect/execute the real host-native fixture bytes.
        fs::remove_file(bin.join("od")).unwrap();
        executable(&bin.join("od"), "#!/bin/sh\nprintf '%s\\n' 'cf fa ed fe 07 00 00 01 03 00 00 00 02 00 00 00 00 00 00 00'\n");
    }
    if case.fault == Some("missing-perl") {
        fs::remove_file(bin.join("perl")).unwrap();
    }
    if matches!(
        case.fault,
        Some("publish-directory" | "publish-file" | "publish-symlink")
    ) {
        let publisher = if case.darwin || cfg!(target_os = "macos") {
            "perl"
        } else {
            "ln"
        };
        let real = fs::read_link(bin.join(publisher)).unwrap();
        fs::remove_file(bin.join(publisher)).unwrap();
        executable(
            &bin.join(publisher),
            &format!(
                r#"#!/bin/sh
printf 'publication boundary\n' > "$MOCK_PUBLISH_LOG"
case "$MOCK_FAULT" in
  publish-directory) mkdir "$MOCK_INSTALL_DIR/muqun-gateway";;
  publish-file) printf 'publication race image\n' > "$MOCK_INSTALL_DIR/muqun-gateway";;
  publish-symlink) /bin/ln -s "$MOCK_BINARY" "$MOCK_INSTALL_DIR/muqun-gateway";;
esac
exec '{}' "$@"
"#,
                real.display()
            ),
        );
    }
    let hash_name = if case.shasum || !Path::new("/usr/bin/sha256sum").exists() {
        "shasum"
    } else {
        "sha256sum"
    };
    let hash_program = [
        format!("/usr/bin/{hash_name}"),
        format!("/usr/bin/core_perl/{hash_name}"),
    ]
    .into_iter()
    .find(|path| Path::new(path).exists())
    .unwrap();
    std::os::unix::fs::symlink(hash_program, bin.join(hash_name)).unwrap();
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
    executable(
        &bin.join("curl"),
        r#"#!/bin/sh
call="$*"
output= url= limit= proto= redir= connect= time= head=0
while [ $# -gt 0 ]; do
  case "$1" in
    -o) output="$2"; shift;;
    --max-filesize) limit="$2"; shift;;
    --proto) proto="$2"; shift;;
    --proto-redir) redir="$2"; shift;;
    --connect-timeout) connect="$2"; shift;;
    --max-time) time="$2"; shift;;
    -w) [ "$2" = '%{url_effective}' ] || exit 91; shift;;
    -fsSLI) head=1;;
    -fsSL|-fSL|--progress-bar) :;;
    https://*) url="$1";;
    *) exit 91;;
  esac
  shift
done
# The installer's per-process file-size limit also applies to fixture logs.
# Keep each request in its own small file rather than one accumulating log.
printf 'curl %s\n' "$call" > "$MOCK_DOWNLOAD_LOG.${output##*/}"
[ "$proto" = '=https' ] && [ "$redir" = '=https' ] &&
  [ "$connect" = 10 ] && [ -n "$time" ] && [ -n "$limit" ] || exit 91
if [ "$head" = 1 ]; then
  [ "$output" = /dev/null ] &&
    [ "$url" = https://github.com/osuki-dev/muqun-gateway/releases/latest ] || exit 91
  [ "$MOCK_FAULT" != resolve ] || exit 22
  printf 'https://github.com/osuki-dev/muqun-gateway/releases/tag/%s' "$MOCK_TAG"
  exit
fi
case "$output" in
  "$MOCK_INSTALL_DIR"/.muqun-gateway-install.??????/*) :;;
  *) exit 91;;
esac
case "$(umask)" in 0077|077) :;; *) exit 91;; esac
[ -d "${output%/*}" ] && [ ! -e "$output" ] || exit 91
# Numeric modes ignore ls metadata markers for extended attributes and ACLs.
# Probe GNU stat first, then BSD stat, without retaining a failed probe's output.
if permissions="$(stat -c '%a' "${output%/*}" 2>/dev/null)"; then
  :
else
  permissions="$(stat -f '%Lp' "${output%/*}")" || exit 91
fi
[ "$permissions" = 700 ] || exit 91
case "$url" in
  "https://github.com/osuki-dev/muqun-gateway/releases/download/$MOCK_TAG/muqun-gateway-"*.sha256)
    [ "$limit" = 1024 ] || exit 91
    [ "$MOCK_FAULT" != missing ] || exit 22
    if [ -x "${0%/*}/sha256sum" ]; then
      hash="$(sha256sum "$MOCK_BINARY")"
    else
      hash="$(shasum -a 256 "$MOCK_BINARY")"
    fi
    hash="${hash%% *}"
    name="${url##*/}"
    name="${name%.sha256}"
    case "$MOCK_FAULT" in
      corrupt) hash=0000000000000000000000000000000000000000000000000000000000000000;;
      wrong-name) name=other;;
      malformed) hash=not-a-sha256;;
      uppercase) hash=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA;;
    esac
    printf '%s  %s\n' "$hash" "$name" > "$output"
    [ "$MOCK_FAULT" != extra-line ] || printf 'extra\n' >> "$output"
    [ "$MOCK_FAULT" != no-newline ] || printf '%s  %s' "$hash" "$name" > "$output"
    [ "$MOCK_FAULT" != empty ] || : > "$output"
    [ "$MOCK_FAULT" != oversized ] || printf '%02048d' 0 > "$output"
    ;;
  "https://github.com/osuki-dev/muqun-gateway/releases/download/$MOCK_TAG/muqun-gateway-"*)
    [ "$limit" = 268435456 ] || exit 91
    cat "$MOCK_BINARY" > "$output"
    [ "$MOCK_FAULT" != download ] || exit 22
    if [ "$MOCK_FAULT" = concurrent ]; then
      printf 'concurrent install\n' > "$MOCK_INSTALL_DIR/muqun-gateway"
    fi
    if [ "$MOCK_FAULT" = concurrent-directory ]; then
      mkdir "$MOCK_INSTALL_DIR/muqun-gateway"
    fi
    ;;
  *) exit 91;;
esac
[ ! -x "$output" ] || exit 91
"#,
    );
    let fake = root.join("gateway");
    let script = root.join("gateway-script");
    executable(
        &script,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$MOCK_LOG"
if [ "${1:-}" = setup ]; then
  printf '%s\000' "$@" > "$MOCK_SETUP_LOG"
  [ "$MOCK_FAULT" != setup ] || exit 1
fi
case "$*" in
'--version')
  case "$MOCK_FAULT" in
    wrong-version) echo 'muqun-gateway 0.12.0';;
    malformed-version) echo 'gateway 0.13.0';;
    nul-version) printf 'muqun-gateway 0.\00013.0\n';;
    extra-version) printf 'muqun-gateway 0.13.0\nextra\n';;
    failed-version) echo 'muqun-gateway 0.13.0'; exit 1;;
    empty-version) :;;
    oversized-version) printf '%02048d' 0;;
    hung-version) exec sleep 30;;
    *) echo 'muqun-gateway 0.13.0';;
  esac;;
'update --help')
  case "$MOCK_EXISTING" in
    modern*) printf '%s\n' 'Check or manually install a newer stable official release (never downgrades).' 'Usage: muqun-gateway update [OPTIONS]' '  --check  Only check; do not download binaries or change running state.';;
    legacy) echo 'Usage: muqun-gateway [OPTIONS] <COMMAND>';;
    empty-help) :;;
    *) exit 2;;
  esac;;
'update') [ "$MOCK_EXISTING" != modern-failed ] || exit 1;;
'service status') echo "service: $MOCK_SERVICE"; echo "gateway: $MOCK_GATEWAY";;
'backend list') printf '%b' "$MOCK_BACKENDS";;
esac
"#,
    );
    let mut image = native_fixture().to_vec();
    if case.fault == Some("wrong-platform") {
        #[cfg(target_os = "linux")]
        {
            image[18] = 0;
            image[19] = 0;
        }
        #[cfg(target_os = "macos")]
        {
            image[4..8].fill(0);
        }
    }
    if case.fault == Some("script-image") {
        image = fs::read(&script).unwrap();
    }
    fs::write(&fake, image).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let destination = install.join("muqun-gateway");
    let old_contents = format!(
        "{}\n# existing image\n",
        fs::read_to_string(&script).unwrap()
    );
    if let Some(existing) = case.existing {
        match existing {
            "symlink" => std::os::unix::fs::symlink(&fake, &destination).unwrap(),
            "directory" => fs::create_dir(&destination).unwrap(),
            _ => executable(&destination, &old_contents),
        }
        if existing == "nonexecutable" {
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    // A historical shared .new path may be somebody else's symlink. It must
    // neither be overwritten nor cleaned by this invocation.
    std::os::unix::fs::symlink(&fake, install.join("muqun-gateway.new")).unwrap();
    let log = root.join("calls.log");
    let download_log = root.join("downloads.log");
    let error_log = root.join("errors.log");
    let setup_log = root.join("setup.args");
    let socket = socket_name.map(|name| root.join(name));
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let mut cmd = if answer.is_some() {
        let mut cmd = Command::new("/usr/bin/script");
        #[cfg(target_os = "macos")]
        cmd.args(["-q", "/dev/null", "/bin/sh", "-c"])
            .arg(format!("stty cols 32 && /bin/sh '{}'", installer.display()));
        #[cfg(not(target_os = "macos"))]
        cmd.args(["-q", "-c"])
            .arg(format!("stty cols 32 && /bin/sh '{}'", installer.display()))
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
        .env("TERM", case.term.unwrap_or("dumb"))
        .env("COLUMNS", "32")
        .env("MUQUN_GATEWAY_INSTALL_DIR", &install)
        .env("MOCK_INSTALL_DIR", &install)
        .env("MOCK_BINARY", &fake)
        .env("MOCK_SCRIPT", &script)
        .env("MOCK_PUBLISH_LOG", root.join("publish.log"))
        .env("MOCK_LOG", &log)
        .env("MOCK_DOWNLOAD_LOG", &download_log)
        .env("MOCK_EXISTING", case.existing.unwrap_or(""))
        .env("MOCK_FAULT", case.fault.unwrap_or(""))
        .env("MOCK_TAG", case.tag.unwrap_or("v0.13.0"))
        .env("MOCK_SETUP_LOG", &setup_log)
        .env("MOCK_BACKENDS", backends)
        .env("MOCK_GATEWAY", gateway)
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
        .stderr(fs::File::create(&error_log).unwrap());
    if case.no_color {
        cmd.env("NO_COLOR", "");
    }
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
    // A gateway that never comes up is waited on for 20 s before the installer
    // gives up, so that case needs the longer budget.
    let budget = if gateway == "running" { 15 } else { 40 };
    let deadline = Instant::now() + Duration::from_secs(budget);
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
    for text in rx.try_iter() {
        transcript.push_str(&text);
    }
    transcript.push_str(&fs::read_to_string(&error_log).unwrap());
    let calls = fs::read_to_string(&log).unwrap_or_default();
    let downloads: String = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("downloads.log.")
        })
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();
    if case.existing.is_none() && answer.is_some() && case.term == Some("xterm") {
        assert_eq!(downloads.contains("--progress-bar"), !case.no_color);
    } else {
        assert!(!downloads.contains("--progress-bar"), "{downloads}");
    }
    assert!(install.join("muqun-gateway.new").is_symlink());
    assert_eq!(
        fs::read(&fake).unwrap(),
        fs::read(install.join("muqun-gateway.new")).unwrap()
    );
    assert!(fs::read_dir(&install).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".muqun-gateway-install.")));
    if let Some(existing) = case.existing {
        assert!(
            downloads.is_empty(),
            "existing install downloaded assets: {downloads}"
        );
        match existing {
            "symlink" => assert!(destination.is_symlink()),
            "directory" => assert!(destination.is_dir()),
            _ => assert_eq!(fs::read_to_string(&destination).unwrap(), old_contents),
        }
    } else if case.fault == Some("concurrent") {
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "concurrent install\n"
        );
    } else if matches!(
        case.fault,
        Some("concurrent-directory" | "publish-directory")
    ) {
        assert!(destination.is_dir());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    } else if case.fault == Some("publish-file") {
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "publication race image\n"
        );
    } else if case.fault == Some("publish-symlink") {
        assert_eq!(fs::read_link(&destination).unwrap(), fake);
    } else if !status.success() && !calls.contains("setup --backend") {
        assert!(!destination.exists(), "unverified binary was installed");
        assert!(
            calls.is_empty() || calls == "--version\n",
            "unverified binary was executed: {calls}"
        );
    }
    if matches!(
        case.fault,
        Some("publish-directory" | "publish-file" | "publish-symlink")
    ) {
        assert_eq!(
            fs::read_to_string(root.join("publish.log")).unwrap(),
            "publication boundary\n"
        );
    }
    if !status.success() {
        // All contents are generated fixtures under this unique test-owned path.
        fs::remove_dir_all(root).unwrap();
        return Err(format!(
            "{calls}\n--- downloads ---\n{downloads}\n--- transcript ---\n{transcript}"
        ));
    }
    if case.existing.is_some() {
        fs::remove_dir_all(root).unwrap();
        return Ok(if case.transcript { transcript } else { calls });
    }
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
    Ok(if case.transcript { transcript } else { calls })
}

#[test]
fn an_update_whose_gateway_never_comes_up_is_reported_as_a_failure() {
    // v0.12.1 printed "updated and running" after a launchd reload that had
    // left nothing running. Both the supervised and the unmanaged path must
    // fail loudly instead, and never claim success.
    for installed in [true, false] {
        let failure = run_installer(true, true, None, installed, None, None, "not running")
            .expect_err("an installer whose gateway never came up must fail");
        // It waited out the whole start-up budget, asking every second.
        let polls = failure
            .lines()
            .filter(|line| *line == "service status")
            .count();
        assert!(polls >= 20, "polled {polls} times:\n{failure}");
        assert!(!failure.contains("updated and running"), "{failure}");
        assert!(!failure.contains("configured and running"), "{failure}");
        assert!(!failure.contains("Summary: Muqun Gateway"), "{failure}");
    }
}

#[test]
fn a_host_that_cannot_tell_whether_the_gateway_runs_still_installs() {
    // `service status` could not answer at all -- no `ss` on a minimal Linux.
    // That is not evidence of a failed start, so the install completes.
    run_installer(true, true, None, true, None, None, "unknown")
        .expect("an unanswerable status check must not fail the install");
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

fn security_case(case: DownloadCase) -> Result<String, String> {
    run_installer_case(false, true, None, false, None, None, "running", case)
}

#[test]
fn installer_verifies_tag_pinned_downloads_before_installing_or_executing() {
    for fault in [
        "resolve",
        "download",
        "missing",
        "corrupt",
        "wrong-name",
        "malformed",
        "uppercase",
        "empty",
        "oversized",
        "extra-line",
        "concurrent",
        "concurrent-directory",
    ] {
        let failure = security_case(DownloadCase {
            fault: Some(fault),
            ..Default::default()
        })
        .expect_err(fault);
        assert!(failure.contains("Error:"), "{fault}: {failure}");
        assert!(!failure.contains("configured and running"), "{failure}");
        if fault != "resolve" {
            assert!(failure.contains("/releases/download/v0.13.0/"), "{failure}");
        }
        assert!(!failure.contains("latest/download"), "{failure}");
    }
    // No final newline is also accepted by the protected updater's checksum
    // parser. Exercise the standard macOS hash tool as well as sha256sum.
    security_case(DownloadCase {
        fault: Some("no-newline"),
        shasum: true,
        ..Default::default()
    })
    .unwrap();
}

#[test]
fn installer_rejects_noncanonical_stable_tags_before_asset_download() {
    for tag in [
        "v0.13.0-rc.1",
        "v0.13.0/other",
        "v0.13.0?download=1",
        "v01.13.0",
        "0.13.0",
        "v0.13.0\nv0.14.0",
        "v0.13.0\nmalicious",
        "",
    ] {
        let failure = security_case(DownloadCase {
            tag: Some(tag),
            ..Default::default()
        })
        .expect_err(tag);
        assert!(failure.contains("stable vX.Y.Z tag"), "{failure}");
        assert!(!failure.contains("/releases/download/"), "{failure}");
    }
}

#[test]
fn installer_delegates_modern_updates_without_shell_restart_or_replacement() {
    let calls = security_case(DownloadCase {
        existing: Some("modern"),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(calls, "update --help\nupdate\n");
    let failure = security_case(DownloadCase {
        existing: Some("modern-failed"),
        ..Default::default()
    })
    .expect_err("failed updater must not fall back to the shell installer");
    assert!(failure.starts_with("update --help\nupdate\n"), "{failure}");
    assert!(failure.contains("Protected update failed"), "{failure}");
    assert!(!failure.contains("service install"), "{failure}");
    assert!(!failure.contains("setup --backend"), "{failure}");
}

#[test]
fn installer_retains_older_images_and_refuses_unsafe_destinations() {
    for existing in [
        "legacy",
        "empty-help",
        "unsupported",
        "nonexecutable",
        "symlink",
        "directory",
    ] {
        for fault in ["corrupt", "missing", "download"] {
            let failure = security_case(DownloadCase {
                existing: Some(existing),
                fault: Some(fault),
                ..Default::default()
            })
            .expect_err(existing);
            if matches!(existing, "symlink" | "directory") {
                assert!(
                    failure.contains("Refusing symlink or nonregular"),
                    "{failure}"
                );
                assert!(!failure.contains("update --help"), "{failure}");
            } else {
                assert!(failure.contains("manual migration"), "{failure}");
                assert_eq!(
                    failure.starts_with("update --help\n"),
                    existing != "nonexecutable"
                );
            }
            assert!(!failure.contains("setup --backend"), "{failure}");
        }
    }
}

#[test]
fn installer_progress_is_plain_and_ordered_in_nonterminal_or_no_color_modes() {
    for (answer, term, no_color) in [
        (None, "xterm", false),
        (Some("n"), "xterm", true),
        (Some("n"), "dumb", false),
    ] {
        let transcript = run_installer_case(
            false,
            true,
            answer,
            false,
            None,
            None,
            "running",
            DownloadCase {
                term: Some(term),
                no_color,
                transcript: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!transcript.contains('\u{1b}'), "{transcript}");
        let stages = [
            "Check platform and backends",
            "Check install destination",
            "Resolve stable release",
            "Download binary and checksum",
            "Verify release identity",
            "Publish verified binary",
            "Configure first-run setup",
            "Start Gateway and ask autostart",
            "Check Gateway readiness",
        ];
        let mut last = 0;
        for (index, title) in stages.iter().enumerate() {
            let start = transcript
                .find(&format!("[{}] {title}", index + 1))
                .unwrap();
            let success = transcript.find(&format!("[{}] OK", index + 1)).unwrap();
            assert!(start >= last && success > start, "{transcript}");
            last = success;
        }
        assert!(transcript.find("Summary:").unwrap() > last, "{transcript}");
        assert!(transcript.contains("Gateway readiness: running."));
        assert!(transcript.contains("Enable backend startup?") || answer.is_none());
    }
}

#[test]
fn installer_progress_reports_only_stages_actually_completed() {
    let failure = security_case(DownloadCase {
        fault: Some("corrupt"),
        ..Default::default()
    })
    .unwrap_err();
    assert!(!failure.contains('\u{1b}'), "{failure}");
    assert!(failure.contains("[4] OK"), "{failure}");
    assert!(failure.contains("[5] FAILED"), "{failure}");
    assert!(!failure.contains("[5] OK"), "{failure}");
    assert!(!failure.contains("[6]"), "{failure}");
    assert!(
        failure.contains("Summary: stopped at stage 5."),
        "{failure}"
    );

    let setup_failure = security_case(DownloadCase {
        fault: Some("setup"),
        ..Default::default()
    })
    .unwrap_err();
    assert!(setup_failure.contains("[7] FAILED"), "{setup_failure}");
    assert!(!setup_failure.contains("[7] OK"), "{setup_failure}");
    assert!(setup_failure.contains("Summary: stopped at stage 7."));

    let modern = security_case(DownloadCase {
        existing: Some("modern"),
        transcript: true,
        ..Default::default()
    })
    .unwrap();
    assert!(modern.contains("[3] Check protected updater"), "{modern}");
    assert!(modern.contains("[4] Run protected update"), "{modern}");
    assert!(modern.contains("[4] OK"), "{modern}");
    assert!(modern.contains("Summary: protected updater finished."));
    assert!(!modern.contains("Download binary"), "{modern}");
    assert!(!modern.contains("Configure first-run"), "{modern}");
    let failure = security_case(DownloadCase {
        existing: Some("modern-failed"),
        ..Default::default()
    })
    .unwrap_err();
    assert!(failure.contains("[4] FAILED"), "{failure}");
    assert!(!failure.contains("[4] OK"), "{failure}");
    assert!(
        failure.contains("Summary: stopped at stage 4."),
        "{failure}"
    );
}

#[test]
fn installer_uses_restrained_terminal_color_and_real_curl_progress_option() {
    let transcript = run_installer_case(
        false,
        true,
        Some("n"),
        false,
        None,
        None,
        "running",
        DownloadCase {
            term: Some("xterm"),
            transcript: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(transcript.contains("\u{1b}[1;36mMuqun Gateway\u{1b}[0m"));
    assert!(transcript.contains("Enable backend startup?"));
    assert!(transcript.contains("Set it up now?"));
}

#[test]
fn installer_rejects_wrong_native_platform_before_execution() {
    for fault in ["wrong-platform", "script-image"] {
        let failure = security_case(DownloadCase {
            fault: Some(fault),
            ..Default::default()
        })
        .unwrap_err();
        assert!(
            failure.contains("does not match this native platform"),
            "{failure}"
        );
        assert!(!failure.contains("--version\n"), "{failure}");
        assert!(failure.contains("[5] FAILED"), "{failure}");
        assert!(!failure.contains("[6]"), "{failure}");
    }
}

#[test]
fn installer_requires_bounded_successful_exact_tag_version_before_publication() {
    for fault in [
        "wrong-version",
        "malformed-version",
        "nul-version",
        "extra-version",
        "failed-version",
        "empty-version",
        "oversized-version",
        "hung-version",
    ] {
        let failure = security_case(DownloadCase {
            fault: Some(fault),
            ..Default::default()
        })
        .unwrap_err();
        assert!(failure.starts_with("--version\n"), "{fault}: {failure}");
        assert!(
            failure.contains("Staged binary version"),
            "{fault}: {failure}"
        );
        assert!(failure.contains("[5] FAILED"), "{failure}");
        assert!(!failure.contains("[6]"), "{failure}");
        assert!(!failure.contains("Installed verified"), "{failure}");
    }
    let failure = security_case(DownloadCase {
        tag: Some("v0.14.0"),
        ..Default::default()
    })
    .unwrap_err();
    assert!(failure.contains("does not match v0.14.0"), "{failure}");
}

#[test]
fn installer_publication_refuses_destinations_created_at_the_atomic_boundary() {
    for darwin in [false, true] {
        for fault in ["publish-directory", "publish-file", "publish-symlink"] {
            let failure = security_case(DownloadCase {
                fault: Some(fault),
                darwin,
                ..Default::default()
            })
            .unwrap_err();
            assert!(failure.starts_with("--version\n"), "{failure}");
            assert!(failure.contains("Cannot publish binary"), "{failure}");
            assert!(failure.contains("[6] FAILED"), "{failure}");
            assert!(!failure.contains("Installed verified"), "{failure}");
            assert!(!failure.contains("setup --backend"), "{failure}");
        }
    }
    // Also prove the direct Perl link() succeeds when the destination remains
    // absent. Darwin's header is modeled on non-macOS hosts, not live-executed.
    security_case(DownloadCase {
        darwin: true,
        ..Default::default()
    })
    .unwrap();
    let failure = security_case(DownloadCase {
        darwin: true,
        fault: Some("missing-perl"),
        ..Default::default()
    })
    .unwrap_err();
    assert!(failure.contains("System Perl is required"), "{failure}");
    assert!(!failure.contains("/releases/download/"), "{failure}");
    assert!(!failure.contains("--version\n"), "{failure}");
}
