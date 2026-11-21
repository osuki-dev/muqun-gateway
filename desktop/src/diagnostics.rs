//! User-visible diagnostics. Only bounded, structured metadata enters the journal.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{mpsc, Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const LIMIT: usize = 200;
static JOURNAL: OnceLock<PathBuf> = OnceLock::new();
static WRITE_LOCK: Mutex<()> = Mutex::new(());
static EVENTS: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());
static SAVE: OnceLock<mpsc::SyncSender<()>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    DesktopStart,
    DesktopReport,
    PluginOpen,
    PluginAction,
    PluginCommand,
    PluginInstall,
    PluginUpdate,
    PluginRemove,
    RuntimeInstall,
}

impl EventKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::DesktopReport => "Desktop diagnostic",
            Self::DesktopStart => "Start Desktop",
            Self::PluginOpen => "Open plugin",
            Self::PluginAction => "Run plugin action",
            Self::PluginCommand => "Run plugin command",
            Self::PluginInstall => "Install plugin",
            Self::PluginUpdate => "Update plugin",
            Self::PluginRemove => "Uninstall plugin",
            Self::RuntimeInstall => "Install runtime",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Warning,
    Error,
    Cancelled,
    TimedOut,
    RuntimeMissing,
    EntryMissing,
    ProcessFailed,
    InvalidResponse,
    PackageManagerFailed,
    SdkIncompatible,
    PackageFailed,
    RuntimeInstallFailed,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Warning => "Warning",
            Self::Error => "Error",
            Self::Completed => "Completed",
            Self::Cancelled => "Cancelled",
            Self::TimedOut => "Timed out",
            Self::RuntimeMissing => "Runtime unavailable",
            Self::EntryMissing => "Entry point missing",
            Self::ProcessFailed => "Plugin process failed",
            Self::InvalidResponse => "Invalid native view response",
            Self::PackageManagerFailed => "Bun package command failed",
            Self::SdkIncompatible => "Incompatible Desktop SDK",
            Self::PackageFailed => "Package operation failed",
            Self::RuntimeInstallFailed => "Runtime installation failed",
        }
    }
    pub fn failed(self) -> bool {
        !matches!(self, Self::Completed | Self::Cancelled | Self::Warning)
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}
impl std::error::Error for Outcome {}

#[derive(Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub timestamp_ms: u64,
    pub duration_ms: u64,
    pub action: EventKind,
    pub outcome: Outcome,
    pub source: String,
    pub version: Option<String>,
    pub desktop_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bun_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default)]
    pub wrote_stderr: bool,
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.+@/".contains(&b))
        && (!s.contains('/') || (s.starts_with('@') && s.matches('/').count() == 1))
}

pub struct Attempt {
    pub bun_version: Option<String>,
    location: Option<String>,
    pub wrote_stderr: bool,
    started: Instant,
    action: EventKind,
    source: String,
    version: Option<String>,
}

impl Attempt {
    pub fn new(action: EventKind, source: &str, version: Option<&str>) -> Self {
        Self {
            started: Instant::now(),
            bun_version: None,
            location: None,
            wrote_stderr: false,
            action,
            source: if identifier(source) { source } else { "Plugin" }.into(),
            version: version.filter(|v| identifier(v)).map(str::to_owned),
        }
    }
    pub fn set_source(&mut self, source: &str) {
        if identifier(source) {
            self.source = source.to_owned();
        }
    }
    pub fn finish(self, outcome: Outcome) -> Uuid {
        let id = Uuid::new_v4();
        let event = Event {
            id,
            action: self.action,
            location: self.location,
            wrote_stderr: self.wrote_stderr,
            outcome,
            source: self.source,
            version: self.version,
            bun_version: self.bun_version.filter(|v| identifier(v)),
            desktop_version: env!("CARGO_PKG_VERSION").into(),
            timestamp_ms: now(),
            duration_ms: self.started.elapsed().as_millis() as u64,
        };
        let mut events = EVENTS.lock().unwrap();
        if events.len() >= LIMIT {
            events.pop_front();
        }
        events.push_back(event);
        drop(events);
        if let Some(save) = SAVE.get() {
            let _ = save.try_send(());
        }
        id
    }
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub fn recent() -> std::sync::Arc<Vec<Event>> {
    static SNAPSHOT: Mutex<Option<std::sync::Arc<Vec<Event>>>> = Mutex::new(None);
    let events = EVENTS.lock().unwrap();
    let mut cached = SNAPSHOT.lock().unwrap();
    if let Some(snapshot) = cached.as_ref() {
        if snapshot.first().map(|e| e.id) == events.back().map(|e| e.id) {
            return snapshot.clone();
        }
    }
    let snapshot = std::sync::Arc::new(events.iter().rev().cloned().collect::<Vec<_>>());
    *cached = Some(snapshot.clone());
    snapshot
}

/// Only the primary application persists history; scenes and tests use memory.
pub fn initialize(dir: PathBuf) {
    let path = dir.join("events-v1.json");
    if fs::metadata(&path).is_ok_and(|m| m.len() < 512 * 1024) {
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(history) = serde_json::from_slice::<VecDeque<Event>>(&bytes) {
                *EVENTS.lock().unwrap() = history
                    .into_iter()
                    .rev()
                    .take(LIMIT)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
            }
        }
    }
    if JOURNAL.set(path).is_err() {
        return;
    }
    let (tx, rx) = mpsc::sync_channel(1);
    if SAVE.set(tx).is_err() {
        return;
    }
    std::thread::spawn(move || {
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        while rx.recv().is_ok() {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let _ = rx.try_recv();
            flush();
        }
    });
}

/// Serialize writes so a final flush cannot be overwritten by an older pending snapshot.
pub fn flush() {
    let Some(path) = JOURNAL.get() else {
        return;
    };
    let _write = WRITE_LOCK.lock().unwrap();
    let snapshot = EVENTS.lock().unwrap().clone();
    if let Ok(bytes) = serde_json::to_vec(&snapshot) {
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(tmp, path);
        }
    }
}

/// Capture the compiled call site only. Tracing fields may contain credentials or conversation data.
pub struct DesktopReports;
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DesktopReports {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        if !meta.target().starts_with("muqun_desktop") {
            return;
        }
        let outcome = match *meta.level() {
            tracing::Level::WARN => Outcome::Warning,
            tracing::Level::ERROR => Outcome::Error,
            _ => return,
        };
        let mut attempt = Attempt::new(EventKind::DesktopReport, "Desktop", None);
        attempt.location = Some(format!(
            "{}:{}",
            meta.module_path().unwrap_or(meta.target()),
            meta.line().unwrap_or_default()
        ));
        attempt.finish(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_does_not_retain_tracing_fields_or_inputs() {
        use tracing_subscriber::prelude::*;
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(DesktopReports),
            || {
                tracing::error!(target:"muqun_desktop::diagnostic_test", secret="example-private-value","sensitive message");
            },
        );
        let records = recent();
        let entry = records
            .iter()
            .find(|e| {
                e.location
                    .as_ref()
                    .is_some_and(|s| s.starts_with(module_path!()))
            })
            .unwrap();
        let report = serde_json::to_string(entry).unwrap();
        assert!(!report.contains("example-private-value"));
        assert!(!report.contains("sensitive message"));
        let id = Attempt::new(
            EventKind::PluginUpdate,
            "/home/private-user/plugin.tgz",
            Some("Bearer secret"),
        )
        .finish(Outcome::PackageFailed);
        let records = recent();
        let entry = records.iter().find(|e| e.id == id).unwrap();
        assert_eq!(entry.source, "Plugin");
        assert_eq!(entry.version, None);
    }
}
