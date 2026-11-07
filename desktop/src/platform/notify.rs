//! System notifications: a session that needs the owner, or finished, while
//! it is not on screen.
//!
//! What a notification says and when one is raised is decided elsewhere
//! (`model/notify.rs`); this module only puts it on the desktop and reports a
//! click back through the callback [`install`] registered, with the
//! notification's key (the session id) and, on Wayland, the activation token
//! the compositor needs before it lets the window take focus.
//!
//! Linux speaks `org.freedesktop.Notifications` over the session bus (mako,
//! dunst, swaync, GNOME Shell, KDE Plasma). macOS and Windows have no backend
//! yet: [`show`] logs and returns, so the rest of the app is the same there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// One notification. `key` identifies what it is about (a session id): a
/// newer notice with the same key replaces the older one on screen, and
/// [`withdraw`] takes it down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub key: String,
    pub title: String,
    pub body: String,
    /// The agent is blocked on the owner (an approval or a question):
    /// asked of the server as critical-ish ("normal" otherwise) and kept on
    /// screen until it is answered or clicked.
    pub waiting: bool,
    /// The label of the notification's one action (servers that draw
    /// buttons show it; a click on the body does the same).
    pub action: String,
}

type OnOpen = Box<dyn Fn(String, Option<String>) + Send + Sync>;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
static ON_OPEN: OnceLock<OnOpen> = OnceLock::new();
static FOCUSED: AtomicBool = AtomicBool::new(false);

/// Register what a click does: `on_open(key, activation_token)`. Called once
/// at startup; runs on the notification thread.
pub fn install(on_open: impl Fn(String, Option<String>) + Send + Sync + 'static) {
    let _ = ON_OPEN.set(Box::new(on_open));
}

/// The window gained or lost keyboard focus (Rune's `on_focus`).
pub fn set_window_focused(focused: bool) {
    FOCUSED.store(focused, Ordering::Release);
}

/// Whether the owner is looking at the window: focused, as the compositor
/// reports it. A window behind another one, minimized or on another
/// workspace is not being looked at.
pub fn window_focused() -> bool {
    FOCUSED.load(Ordering::Acquire)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn clicked(key: String, token: Option<String>) {
    match ON_OPEN.get() {
        Some(on_open) => on_open(key, token),
        None => tracing::debug!("a notification was clicked before the app listened"),
    }
}

/// Put `notice` on the desktop. Never blocks; failures are logged.
pub fn show(notice: Notice) {
    backend::send(backend::Job::Show(notice));
}

/// Take down the notification for `key`, if one is up.
pub fn withdraw(key: &str) {
    backend::send(backend::Job::Withdraw(key.to_string()));
}

#[cfg(target_os = "linux")]
mod backend {
    //! `org.freedesktop.Notifications` on its own thread, with the
    //! connection made on first use and kept.

    use std::collections::HashMap;
    use std::sync::OnceLock;

    use futures::StreamExt as _;
    use tokio::sync::mpsc;
    use zbus::Variant as Value;
    use zbus::{Connection, Proxy};

    use super::Notice;

    const DEST: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";
    const INTERFACE: &str = "org.freedesktop.Notifications";

    pub enum Job {
        Show(Notice),
        Withdraw(String),
    }

    static JOBS: OnceLock<Option<mpsc::UnboundedSender<Job>>> = OnceLock::new();

    pub fn send(job: Job) {
        let jobs = JOBS.get_or_init(start);
        if let Some(jobs) = jobs {
            let _ = jobs.send(job);
        }
    }

    fn start() -> Option<mpsc::UnboundedSender<Job>> {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("muqun-notify".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    if let Err(err) = run(rx).await {
                        tracing::warn!("system notifications are unavailable: {err}");
                    }
                });
            })
            .map_err(|err| tracing::warn!("could not start the notification thread: {err}"))
            .ok()?;
        Some(tx)
    }

    /// Which notification is up for which key, both ways.
    #[derive(Default)]
    struct Shown {
        by_key: HashMap<String, u32>,
        by_id: HashMap<u32, String>,
        /// Activation tokens arrive just before the action they belong to.
        tokens: HashMap<u32, String>,
    }

    impl Shown {
        fn insert(&mut self, key: String, id: u32) {
            if let Some(old) = self.by_key.insert(key.clone(), id) {
                if old != id {
                    self.by_id.remove(&old);
                }
            }
            self.by_id.insert(id, key);
        }

        fn forget(&mut self, id: u32) -> Option<String> {
            self.tokens.remove(&id);
            let key = self.by_id.remove(&id)?;
            if self.by_key.get(&key) == Some(&id) {
                self.by_key.remove(&key);
            }
            Some(key)
        }
    }

    async fn run(mut jobs: mpsc::UnboundedReceiver<Job>) -> zbus::Result<()> {
        let conn = Connection::session().await?;
        let server = Proxy::new(&conn, DEST, PATH, INTERFACE).await?;
        let mut actions = server.receive_signal("ActionInvoked").await?;
        let mut tokens = server.receive_signal("ActivationToken").await?;
        let mut closed = server.receive_signal("NotificationClosed").await?;
        let mut shown = Shown::default();
        loop {
            tokio::select! {
                job = jobs.recv() => match job {
                    Some(Job::Show(notice)) => {
                        let replaces = shown.by_key.get(&notice.key).copied().unwrap_or(0);
                        match notify(&server, &notice, replaces).await {
                            Ok(id) => shown.insert(notice.key, id),
                            Err(err) => tracing::warn!("notification not shown: {err}"),
                        }
                    }
                    Some(Job::Withdraw(key)) => {
                        if let Some(id) = shown.by_key.get(&key).copied() {
                            shown.forget(id);
                            let _ = server.call_method("CloseNotification", &(id,)).await;
                        }
                    }
                    None => return Ok(()),
                },
                Some(message) = tokens.next() => {
                    if let Ok((id, token)) = message.body().deserialize::<(u32, String)>() {
                        shown.tokens.insert(id, token);
                    }
                }
                Some(message) = actions.next() => {
                    if let Ok((id, _action)) = message.body().deserialize::<(u32, String)>() {
                        let token = shown.tokens.remove(&id);
                        if let Some(key) = shown.forget(id) {
                            super::clicked(key, token);
                        }
                    }
                }
                Some(message) = closed.next() => {
                    if let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                        shown.forget(id);
                    }
                }
            }
        }
    }

    async fn notify(server: &Proxy<'_>, notice: &Notice, replaces: u32) -> zbus::Result<u32> {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from(crate::branding::APP_ID));
        hints.insert("category", Value::from("im.received"));
        hints.insert("urgency", Value::U8(if notice.waiting { 2 } else { 1 }));
        // "default" is the click on the notification itself; servers that
        // draw buttons show the label.
        let actions = vec!["default", notice.action.as_str()];
        // 0 never expires, which is what a blocked agent wants; -1 leaves a
        // finished turn to the server's own timeout.
        let timeout: i32 = if notice.waiting { 0 } else { -1 };
        server
            .call(
                "Notify",
                &(
                    crate::branding::APP_NAME,
                    replaces,
                    crate::branding::APP_ID,
                    notice.title.as_str(),
                    notice.body.as_str(),
                    actions,
                    hints,
                    timeout,
                ),
            )
            .await
    }
}

#[cfg(not(target_os = "linux"))]
mod backend {
    use super::Notice;

    pub enum Job {
        Show(Notice),
        Withdraw(String),
    }

    pub fn send(job: Job) {
        if let Job::Show(notice) = job {
            tracing::debug!(
                title = %notice.title,
                "system notifications are not implemented on this platform"
            );
        }
    }
}
