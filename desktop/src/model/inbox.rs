//! Core → UI. The tokio side pushes [`Update`]s here; the main thread takes
//! them in one batch per frame (`drain.rs`). Nothing in here touches Blinc
//! state or builds UI -- the only Blinc call is the wake-up.
//!
//! Coalescing, so a burst of streaming deltas costs one rebuild per item per
//! frame instead of one per delta:
//!
//! * Events carrying a `seq` are dropped when they are not newer than the
//!   last one seen for their session -- a snapshot counts as having seen
//!   everything up to its own `seq`.
//! * Within one batch, a `TimelineUpsert` of an item that an earlier upsert
//!   in the same batch already carries replaces that earlier copy in place:
//!   only the newest version of each item is delivered, at the position the
//!   item first appeared.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use muqun_gateway::agents::directories::DirectoryEntry;
use muqun_gateway::agents::ports::AgentSessionSnapshot;
use muqun_gateway::agents::{AgentDomainEvent, AgentProject, AgentSessionInfo};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;

/// An error the UI shows in its alert strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiError {
    pub message: String,
    /// A second line: what to check (for an unreachable agent, which binary
    /// the desktop would start).
    pub detail: Option<String>,
    /// The agent could not be reached; a later successful read clears it.
    pub unreachable: bool,
}

/// Work that originates on the UI thread but must run after the current
/// input has been fully dispatched (see `ui/composer.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    SubmitComposer,
    /// The composer's text or caret may have changed (posted from its key
    /// handlers, run once the keystroke has reached the text area).
    ComposerEdited,
    /// A key the composer's popup or history wants, with the caret as it was
    /// before the text area saw the key (it moves or inserts on its own).
    ComposerKey {
        key: u32,
        line: usize,
        column: usize,
    },
    /// A folder came back from the system picker (which may run on its own
    /// thread): the start page's project.
    ProjectPicked(std::path::PathBuf),
    /// One step of the composer's QA script (`MUQUN_QA_COMPOSER`).
    QaStep(String),
    /// A PNG came off the clipboard (read on its own thread).
    ImagePasted(Vec<u8>),
    /// Files came back from the attach dialog.
    FilesPicked(Vec<std::path::PathBuf>),
    /// Folders were dropped on the window: their paths go into the draft.
    FoldersDropped(Vec<std::path::PathBuf>),
    /// A file drag entered (`true`) or left the window.
    DropHover(bool),
    /// Open a session as a click on its row would (QA, `MUQUN_QA_SWITCH`).
    Open(String),
    /// Toggle the top-bar working-tree file list.
    ToggleDiffPanel,
    /// Open one changed file in the preferred editor.
    OpenDiffFile {
        asid: String,
        path: String,
    },
}

#[derive(Debug, Clone)]
pub enum Update {
    Agents(Vec<AgentDiscoveryInfo>),
    /// One agent's session list, replacing what was listed for it.
    AgentSessions {
        agent: String,
        sessions: Vec<AgentSessionInfo>,
    },
    SessionCreated(Box<AgentSessionInfo>),
    /// The session was deleted locally; drop it from the list and clear if open.
    SessionGone(String),
    Snapshot(Box<AgentSessionSnapshot>),
    Event(Box<AgentDomainEvent>),
    Error(UiError),
    Projects {
        agent: String,
        projects: Vec<AgentProject>,
    },
    Completions {
        prefix: String,
        entries: Vec<DirectoryEntry>,
    },
    Ui(UiAction),
    /// An older page of a session's history, oldest first. `exhausted`:
    /// the agent has nothing older.
    Earlier {
        asid: String,
        items: Vec<muqun_gateway::agents::TimelineItem>,
        exhausted: bool,
    },
    /// The composer's, applied by `ui/composer/state.rs` on the main thread.
    Composer(Box<super::compose::ComposerUpdate>),
    /// Themes, applied by `ui/themes.rs` on the main thread.
    Theme(Box<crate::theme::service::ThemeUpdate>),
    /// Session folders resolved to their projects (`projects.rs`).
    Directories(Vec<(String, super::projects::DirInfo)>),
    /// One agent's own project list, for the sidebar's projects.
    KnownProjects {
        agent: String,
        projects: Vec<AgentProject>,
    },
    /// The open session's working-tree change count.
    Diff(super::view::DiffStat),
    /// The delegated task list, replacing the last, with the folder a new
    /// task would run in.
    Tasks {
        rows: Vec<crate::hub::TaskRow>,
        default_cwd: Option<String>,
    },
    /// The sessions below `asid`, newest first, replacing the last read.
    Descendants {
        asid: String,
        sessions: Vec<AgentSessionInfo>,
    },
    /// Remembered "always allow" decisions for the open session's project.
    SavedPermissions {
        asid: String,
        items: Vec<crate::hub::SavedPermission>,
    },
    /// Background shells for the open session's agent/folder.
    Shells(Vec<crate::hub::ShellRow>),
    /// Worktrees under the open session's project folder.
    Worktrees(Vec<crate::hub::WorktreeRow>),
    /// Where this desktop's sessions run, applied by the drain to a binding.
    HubMode(crate::hub::HubMode),
}

impl Update {
    /// A domain event; boxed because it is by far the largest variant.
    pub fn event(event: AgentDomainEvent) -> Self {
        Self::Event(Box::new(event))
    }
}

#[derive(Default)]
struct State {
    pending: Vec<Update>,
    /// (asid, item id) → (index in `pending`, index in that upsert's items).
    upserts: HashMap<(String, String), (usize, usize)>,
    /// The newest `seq` seen per session.
    last_seq: HashMap<String, u64>,
}

pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub struct Inbox {
    state: Mutex<State>,
    wake: Mutex<Option<Wake>>,
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Inbox {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            wake: Mutex::new(None),
        }
    }

    /// What to call after every push. Set once the main thread is ready to
    /// drain; pushes before that simply wait in the queue.
    pub fn set_wake(&self, wake: Wake) {
        *self.wake.lock().unwrap() = Some(wake);
    }

    pub fn push(&self, update: Update) {
        let kept = {
            let mut state = self.state.lock().unwrap();
            state.accept(update)
        };
        if kept {
            self.wake();
        }
    }

    fn wake(&self) {
        let wake = self.wake.lock().unwrap().clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    /// Everything queued, oldest first.
    pub fn take(&self) -> Vec<Update> {
        let mut state = self.state.lock().unwrap();
        state.upserts.clear();
        std::mem::take(&mut state.pending)
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().unwrap().pending.is_empty()
    }
}

impl State {
    /// Queue `update`, coalesced. False when it was dropped as stale or
    /// folded entirely into an earlier entry.
    fn accept(&mut self, update: Update) -> bool {
        match update {
            Update::Snapshot(snapshot) => {
                // The snapshot is the truth as of its `seq`; it also resets a
                // session whose mirror restarted its numbering.
                self.last_seq
                    .insert(snapshot.info.asid.0.clone(), snapshot.seq);
                self.pending.push(Update::Snapshot(snapshot));
                true
            }
            Update::Event(event) => self.accept_event(*event),
            other => {
                self.pending.push(other);
                true
            }
        }
    }

    fn accept_event(&mut self, event: AgentDomainEvent) -> bool {
        let seq = event.seq();
        let asid = event.asid().0.clone();
        if seq != 0 {
            let last = self.last_seq.entry(asid.clone()).or_insert(0);
            if seq <= *last {
                return false;
            }
            *last = seq;
        }
        let AgentDomainEvent::TimelineUpsert {
            asid: id,
            items,
            seq,
        } = event
        else {
            self.pending.push(Update::event(event));
            return true;
        };
        let mut fresh = Vec::with_capacity(items.len());
        for item in items {
            match self.upserts.get(&(asid.clone(), item.id.clone())) {
                Some(&(at, index)) => {
                    if let Some(Update::Event(event)) = self.pending.get_mut(at) {
                        if let AgentDomainEvent::TimelineUpsert { items: earlier, .. } =
                            &mut **event
                        {
                            earlier[index] = item;
                        }
                    }
                }
                None => fresh.push(item),
            }
        }
        if fresh.is_empty() {
            return false;
        }
        let at = self.pending.len();
        for (index, item) in fresh.iter().enumerate() {
            self.upserts
                .insert((asid.clone(), item.id.clone()), (at, index));
        }
        self.pending
            .push(Update::event(AgentDomainEvent::TimelineUpsert {
                asid: id,
                items: fresh,
                seq,
            }));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::{
        AgentPart, AgentSessionId, AgentSessionStatus, TimelineItem, TimelineRole,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn item(id: &str, text: &str) -> TimelineItem {
        TimelineItem {
            id: id.into(),
            message_id: "msg_1".into(),
            role: TimelineRole::Assistant,
            part: AgentPart::Text { text: text.into() },
            seq: 0,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        }
    }

    fn upsert(asid: &str, seq: u64, items: Vec<TimelineItem>) -> Update {
        Update::event(AgentDomainEvent::TimelineUpsert {
            asid: AgentSessionId(asid.into()),
            items,
            seq,
        })
    }

    fn texts(update: &Update) -> Vec<String> {
        match update {
            Update::Event(event) => match &**event {
                AgentDomainEvent::TimelineUpsert { items, .. } => items
                    .iter()
                    .map(|i| match &i.part {
                        AgentPart::Text { text } => format!("{}={text}", i.id),
                        _ => unreachable!(),
                    })
                    .collect(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_burst_of_deltas_for_one_item_delivers_only_the_newest_in_place() {
        let inbox = Inbox::new();
        inbox.push(upsert("s", 1, vec![item("a", "H")]));
        inbox.push(upsert("s", 2, vec![item("b", "x")]));
        inbox.push(upsert("s", 3, vec![item("a", "He")]));
        inbox.push(upsert("s", 4, vec![item("a", "Hel"), item("c", "y")]));
        let batch = inbox.take();
        assert_eq!(batch.len(), 3, "the folded upsert adds no entry");
        assert_eq!(texts(&batch[0]), ["a=Hel"]);
        assert_eq!(texts(&batch[1]), ["b=x"]);
        assert_eq!(texts(&batch[2]), ["c=y"]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn coalescing_is_per_batch_and_per_session() {
        let inbox = Inbox::new();
        inbox.push(upsert("s", 1, vec![item("a", "1")]));
        inbox.push(upsert("t", 1, vec![item("a", "other")]));
        assert_eq!(inbox.take().len(), 2, "same id, different sessions");
        inbox.push(upsert("s", 2, vec![item("a", "2")]));
        let batch = inbox.take();
        assert_eq!(texts(&batch[0]), ["a=2"], "a new batch starts fresh");
    }

    #[test]
    fn stale_and_replayed_events_are_dropped() {
        let inbox = Inbox::new();
        let status = |seq| {
            Update::event(AgentDomainEvent::StatusChanged {
                asid: AgentSessionId("s".into()),
                status: AgentSessionStatus::Busy,
                error: None,
                seq,
            })
        };
        inbox.push(status(5));
        inbox.push(status(5));
        inbox.push(status(4));
        inbox.push(status(6));
        assert_eq!(inbox.take().len(), 2);
    }

    #[test]
    fn a_snapshot_marks_everything_up_to_its_seq_as_seen() {
        let inbox = Inbox::new();
        let snapshot = AgentSessionSnapshot {
            info: serde_json::from_value(serde_json::json!({
                "asid": "s", "title": "t", "status": "idle", "updated_ms": 1
            }))
            .unwrap(),
            timeline: vec![],
            permissions: vec![],
            forms: vec![],
            inbox: vec![],
            seq: 10,
        };
        inbox.push(Update::Snapshot(Box::new(snapshot)));
        inbox.push(upsert("s", 9, vec![item("a", "old")]));
        inbox.push(upsert("s", 11, vec![item("a", "new")]));
        let batch = inbox.take();
        assert_eq!(batch.len(), 2);
        assert_eq!(texts(&batch[1]), ["a=new"]);
    }

    #[test]
    fn unsequenced_events_always_pass_and_every_kept_push_wakes() {
        let inbox = Inbox::new();
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = woken.clone();
        inbox.set_wake(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let resync = || {
            Update::event(AgentDomainEvent::Resync {
                asid: AgentSessionId(String::new()),
                reason: "lagged".into(),
            })
        };
        inbox.push(resync());
        inbox.push(resync());
        inbox.push(upsert("s", 1, vec![item("a", "1")]));
        inbox.push(upsert("s", 1, vec![item("a", "dup")]));
        assert_eq!(inbox.take().len(), 3);
        assert_eq!(
            woken.load(Ordering::SeqCst),
            3,
            "a dropped push does not wake"
        );
    }
}
