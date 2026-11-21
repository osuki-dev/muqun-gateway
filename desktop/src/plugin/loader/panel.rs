//! A plugin-owned HTTP service lives exactly as long as its native view instance.
use super::*;
use std::io::{BufRead, BufReader};
use std::process::Child;

const START_TIMEOUT: Duration = Duration::from_secs(45);

pub(super) struct Panel {
    draft_target: Option<crate::plugin::draft::Target>,
    entry: PathBuf,
    events: mpsc::Receiver<Result<String, String>>,
    _lifetime: mpsc::Sender<()>,
    ready: bool,
}

fn panel_entry(dir: &Path, entry: &str) -> Result<PathBuf, String> {
    let entry = Path::new(entry);
    if entry.is_absolute()
        || !entry
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err("Expected a package-relative WebView service entry".into());
    }
    let root = dir
        .canonicalize()
        .map_err(|_| "Plugin directory unavailable")?;
    let script = root
        .join(entry)
        .canonicalize()
        .map_err(|_| "WebView service entry unavailable")?;
    if !script.starts_with(&root) || !script.is_file() {
        return Err("WebView service entry must be a file inside the package".into());
    }
    Ok(script)
}

fn read_endpoint(pipe: impl Read) -> Result<String, String> {
    let mut line = Vec::new();
    let mut reader = BufReader::new(pipe).take(8193);
    reader
        .read_until(b'\n', &mut line)
        .map_err(|_| "Could not read WebView endpoint")?;
    if line.len() > 8192 || line.pop() != Some(b'\n') {
        return Err("WebView service returned no valid endpoint".into());
    }
    let raw = std::str::from_utf8(&line)
        .map_err(|_| "Invalid WebView endpoint")?
        .trim_end_matches('\r');
    if let Some(code) = raw.strip_prefix("!muqun:webview:error:") {
        return Err(match code {
            "runtime-unavailable" => "The plugin runtime is missing. Check the installed package.",
            "backend-unavailable" => "The plugin backend could not open its WebView panel.",
            "invalid-endpoint" => "The plugin returned an invalid local WebView address.",
            _ => "The plugin could not start its WebView service.",
        }
        .into());
    }
    if !super::web_surface::local_panel_url(raw) {
        return Err("WebView service must return a loopback HTTP endpoint".into());
    }
    Ok(raw.to_string())
}

// Keep the Unix group leader reserved until its children have been signalled.
// Reaping the leader with try_wait first would allow the kernel to reuse its PID.
#[cfg(target_os = "linux")]
fn running(child: &mut Child) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0
            && info.assume_init().si_pid() == 0
    }
}

#[cfg(not(target_os = "linux"))]
fn running(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

fn stop(mut child: Child) {
    // The service contract uses EOF for orderly shutdown of its own backend.
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(2);
    while running(&mut child) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl Panel {
    fn start(entry: PathBuf, mut command: ProcessCommand) -> Self {
        let (tx, events) = mpsc::channel();
        let (lifetime, cancel) = mpsc::channel();
        std::thread::spawn(move || {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let mut child = match command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(_) => {
                    let _ = tx.send(Err("Could not start WebView service".into()));
                    return;
                }
            };
            let (startup_tx, startup) = mpsc::channel();
            let stdout = child.stdout.take().unwrap();
            std::thread::spawn(move || {
                let mut output = BufReader::new(stdout);
                let _ = startup_tx.send(read_endpoint(&mut output));
                // The private URL and any further provider output stay out of logs.
                let _ = std::io::copy(&mut output, &mut std::io::sink());
            });
            let deadline = Instant::now() + START_TIMEOUT;
            let mut ready = false;
            let result = loop {
                if !matches!(
                    cancel.recv_timeout(Duration::from_millis(if ready { 250 } else { 30 })),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    break Ok(());
                }
                if !running(&mut child) {
                    let message = if ready {
                        None
                    } else {
                        startup
                            .recv_timeout(Duration::from_millis(100))
                            .ok()
                            .and_then(Result::err)
                    };
                    break Err(message.unwrap_or_else(|| "WebView service stopped".into()));
                }
                if !ready {
                    match startup.try_recv() {
                        Ok(Ok(url)) => {
                            if tx.send(Ok(url)).is_err() {
                                break Ok(());
                            }
                            ready = true;
                        }
                        Ok(Err(message)) => break Err(message),
                        Err(mpsc::TryRecvError::Disconnected) => {
                            break Err("WebView service closed its output".into())
                        }
                        Err(mpsc::TryRecvError::Empty) if Instant::now() >= deadline => {
                            break Err("WebView service startup timed out".into())
                        }
                        Err(_) => {}
                    }
                }
            };
            if let Err(error) = result {
                let _ = tx.send(Err(error));
            }
            stop(child);
        });
        Self {
            draft_target: None,
            entry,
            events,
            _lifetime: lifetime,
            ready: false,
        }
    }
}

impl ExternalPlugin {
    pub(super) fn move_panel_service(&self, from: &str, to: &str) {
        let mut panels = self.panels.lock().unwrap();
        if let Some(panel) = panels.remove(from) {
            panels.insert(to.to_owned(), panel);
        }
    }

    pub(super) fn has_panel_service(&self, instance: &str) -> bool {
        self.panels.lock().unwrap().contains_key(instance)
    }

    pub(super) fn start_panel(
        &self,
        s: &UiState,
        instance: &str,
        entry: &str,
        draft_target: Option<crate::plugin::draft::Target>,
    ) -> Result<(), String> {
        if super::web_surface::webview_helper().is_none() {
            return Err("The embedded WebView helper is unavailable".into());
        }
        let entry = panel_entry(&self.dir, entry)?;
        let mut panels = self.panels.lock().unwrap();
        if panels.get(instance).is_some_and(|p| p.entry == entry) {
            return Ok(());
        }
        let bun = self.resolve_bun_binary().ok_or("Bun runtime required")?;
        let mut command = ProcessCommand::new(bun);
        command
            .arg("run")
            .arg(&entry)
            .current_dir(&self.dir)
            .env("MUQUN_PLUGIN_ID", self.id)
            .env(
                "MUQUN_WORKSPACE_CWD",
                s.current()
                    .and_then(|i| i.directory.clone())
                    .unwrap_or_else(|| s.terminal_cwd().display().to_string()),
            );
        if let Ok(path) = std::env::current_exe() {
            command.env("MUQUN_DESKTOP_EXECUTABLE", path);
        }
        let context = crate::plugin::context::web_context(s, true);
        command.env(
            "MUQUN_PLUGIN_CONTEXT",
            serde_json::to_string(&context).map_err(|_| "Could not encode the WebView context")?,
        );
        self.clear_web_instance(instance);
        let mut panel = Panel::start(entry, command);
        panel.draft_target = draft_target;
        panels.insert(instance.to_string(), panel);
        Ok(())
    }

    pub(super) fn poll_panels(&self, s: &mut UiState, tx: &mut rune::Tx) -> bool {
        let mut changed = false;
        let mut panels = self.panels.lock().unwrap();
        panels.retain(|instance, panel| {
            let result = match panel.events.try_recv() {
                Ok(event) => event.and_then(|url| {
                    self.open_web_url(s, instance, &url, true, panel.draft_target.clone())
                }),
                Err(mpsc::TryRecvError::Disconnected) => Err("WebView service stopped".to_owned()),
                Err(mpsc::TryRecvError::Empty) => return true,
            };
            changed = true;
            match result {
                Ok(()) => {
                    panel.ready = true;
                    true
                }
                Err(error) => {
                    self.clear_web_instance(instance);
                    self.native_views.lock().unwrap().remove(instance);
                    s.toast_error(self.manifest.name.clone(), Some(error));
                    false
                }
            }
        });
        if !panels.is_empty() {
            let waiting = panels.values().any(|p| !p.ready);
            tx.wake_after(Duration::from_millis(if waiting { 30 } else { 1000 }));
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn closing_a_ready_panel_releases_its_service_on_eof() {
        let signal =
            std::env::temp_dir().join(format!("muqun-panel-stop-{}", uuid::Uuid::new_v4()));
        let mut command = ProcessCommand::new("sh");
        command
            .args([
                "-c",
                "printf 'http://127.0.0.1:1242/ready\\n'; cat >/dev/null; printf closed > \"$1\"",
                "panel",
            ])
            .arg(&signal);
        let panel = Panel::start(PathBuf::from("test-entry"), command);
        assert!(panel
            .events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_ok());
        assert!(!signal.exists());
        drop(panel);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !signal.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&signal).unwrap(), "closed");
        std::fs::remove_file(signal).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn immediate_failure_reports_safe_startup_reason() {
        let mut command = ProcessCommand::new("sh");
        command.args([
            "-c",
            "echo '!muqun:webview:error:runtime-unavailable'; exit 1",
        ]);
        let panel = Panel::start(PathBuf::from("test-entry"), command);
        let result = panel
            .events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(result.contains("runtime is missing"));
        let secret = b"!muqun:webview:error:http://127.0.0.1:9887/secret-token\n";
        assert!(!read_endpoint(secret.as_slice())
            .unwrap_err()
            .contains("secret-token"));
    }

    #[test]
    fn only_private_loopback_service_endpoints_are_accepted() {
        assert!(read_endpoint(&b"http://127.0.0.1:5411/private/path\n"[..]).is_ok());
        for value in [
            "http://127.0.0.1.example.net/path\n",
            "http://token@127.0.0.1/path\n",
            "file:///etc/hosts\n",
            "http://127.0.0.1/truncated",
        ] {
            assert!(read_endpoint(value.as_bytes()).is_err());
        }
    }
}
