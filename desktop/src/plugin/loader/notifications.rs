//! Optional package-owned notification poll. The selected view and its worker
//! have separate lifetimes: background reads never replace a form or take focus.
use super::*;

#[derive(Default)]
pub(super) struct Poll {
    pub armed: bool,
    pub pending: Option<String>,
    due: Option<Instant>,
    quiet_next: bool,
    worker: Option<Worker>,
}

impl Poll {
    pub(super) fn new(armed: bool) -> Self {
        Self {
            armed,
            ..Default::default()
        }
    }
}

struct Worker {
    result: mpsc::Receiver<Vec<Notice>>,
    cancel: Arc<AtomicBool>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

#[derive(serde::Deserialize)]
struct Notice {
    title: String,
    body: String,
    action: String,
}

#[derive(serde::Deserialize)]
struct Response {
    notices: Vec<Notice>,
}

const INTERVAL: Duration = Duration::from_secs(30);

impl ExternalPlugin {
    pub(super) fn poll_notifications(&self, s: &mut UiState, tx: &mut rune::Tx) -> bool {
        let Some(entry) = &self.manifest.notification_entry else {
            return false;
        };
        let mut poll = self.notifications.lock().unwrap();
        if poll.pending.is_some() && s.active_plugin_page.as_deref() != Some(self.id) {
            poll.pending = None;
        }
        if poll.pending.is_some() && !self.jobs.lock().unwrap().contains_key(&Some("page".into())) {
            let action = poll.pending.take().unwrap();
            drop(poll);
            return self.on_view_action(s, "page", &action);
        }
        if !poll.armed || s.demo {
            return false;
        }
        if !s.cfg.notifications.enabled {
            poll.worker.take();
            poll.due = None;
            poll.quiet_next = true;
            return false;
        }
        let now = Instant::now();
        if let Some(worker) = &poll.worker {
            let notices = match worker.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => {
                    tx.wake_after(Duration::from_millis(120));
                    return false;
                }
                Err(mpsc::TryRecvError::Disconnected) => None,
            };
            poll.worker.take();
            poll.due = Some(now + INTERVAL);
            tx.wake_after(INTERVAL);
            let Some(notices) = notices else {
                return false;
            };
            if std::mem::take(&mut poll.quiet_next) {
                return false;
            }
            let mut changed = false;
            for notice in notices.into_iter().take(5) {
                if notice.title.is_empty() || notice.action.is_empty() || notice.action.len() > 4096
                {
                    continue;
                }
                let key = format!("plugin:{}", serde_json::json!([self.id, notice.action]));
                let title = format!(
                    "{} · {}",
                    self.manifest.name,
                    notice.title.chars().take(240).collect::<String>()
                );
                let body = notice.body.chars().take(500).collect::<String>();
                s.add_toast(
                    ToastLevel::Info,
                    title.clone(),
                    Some(body.clone()),
                    Some(key.clone()),
                );
                if !crate::platform::notify::window_focused() {
                    crate::platform::notify::show(crate::platform::notify::Notice {
                        key,
                        title,
                        body,
                        waiting: false,
                        action: crate::ui::copy::tr_ref(s.cfg.general.language, "Open").to_owned(),
                    });
                }
                changed = true;
            }
            return changed;
        }
        if let Some(due) = poll.due.filter(|due| *due > now) {
            tx.wake_after(due - now);
            return false;
        }
        poll.due = Some(now + INTERVAL);
        tx.wake_after(INTERVAL);
        let Some(bun) = self.resolve_bun_binary() else {
            return false;
        };
        let mut cmd = ProcessCommand::new(bun);
        cmd.arg("run")
            .arg(self.dir.join(entry))
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (send, result) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        poll.worker = Some(Worker {
            result,
            cancel: cancel.clone(),
        });
        std::thread::spawn(move || {
            let notices = run_script(cmd, cancel)
                .ok()
                .and_then(|out| serde_json::from_str::<Response>(&out.body).ok());
            if let Some(result) = notices {
                let _ = send.send(result.notices);
            }
        });
        tx.wake_after(Duration::from_millis(120));
        false
    }
}
