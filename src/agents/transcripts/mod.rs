//! Read-only transcripts of terminal agents, served as timeline pages.
//!
//! Some agents run in a terminal pane and keep no scrollback a backend can
//! page (Claude Code repaints its screen), yet write their whole conversation
//! to a JSONL file on disk. This module reads those files and projects them
//! into the same [`TimelineItem`]s the agent-session routes serve, so the App
//! renders a terminal agent's history exactly like an OpenCode or Codex
//! session's. Agents with a native adapter (OpenCode, Codex, T3, DeepSeek)
//! keep using it; this layer is only for terminal agents.
//!
//! The layering:
//!
//! * [`TranscriptReader`]: one implementation per agent -- where its files
//!   live, which session ids it accepts, how one line is classified while
//!   indexing, and how a page of lines becomes timeline items. Claude Code
//!   ([`claude::ClaudeReader`]) is the first.
//! * `fence`: the path fence. A reader proposes a file under its own root;
//!   only a regular, non-symlink file inside that root is opened, read-only.
//! * `index`: agent-agnostic line-offset index, its bounded cache, tail
//!   paging with stable cursors, the `head` probe, and malformed-line
//!   accounting.
//! * `routes`: `GET /api/agents/transcripts/{agent}/{session_id}` and
//!   `.../head`, keyed by Herdr's `agent_session.agent` value.
//!
//! ## Adding a reader
//!
//! Write a module implementing [`TranscriptReader`] and add one entry to
//! [`READERS`]. Nothing else changes: the routes, index, paging, cursors,
//! capability list (`agent_transcripts` in `/health` and `/api/discovery`)
//! and the App's renderer all work from the trait. A reader whose format is
//! not one record per line (Aider's markdown history, say) classifies each
//! line as part of a group and projects the group's lines together.

pub(crate) mod claude;
mod fence;
mod index;
pub(crate) mod routes;

use std::path::{Path, PathBuf};

use crate::agents::domain::TimelineItem;

/// What the caller knows about where a session's transcript lives. Hints
/// narrow the search; they never widen the fence.
#[derive(Debug, Default, Clone)]
pub(crate) struct LocateHints {
    /// The pane's working directory, as the terminal backend reports it.
    pub cwd: Option<String>,
}

/// How the index sees one line, decided while indexing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LineClass {
    /// The line yields timeline items on its own and counts toward a page.
    pub visible: bool,
    /// Consecutive visible lines with the same group are one message (an
    /// assistant reply written a content block per line). They share the
    /// first line's message id.
    pub group: Option<String>,
    /// Keys this line answers, looked up by later projections: Claude's
    /// `tool_result` lines answer their `tool_use_id`.
    pub answers: Vec<String>,
}

/// One line handed to a projection.
#[derive(Debug, Clone)]
pub(crate) struct SourceLine {
    /// Zero-based line number in the file; stable while the file only grows.
    pub line: u64,
    /// The line that opened this line's group (itself when ungrouped).
    pub group_start: u64,
    pub bytes: Vec<u8>,
}

/// Lines a projection may look up beyond its page.
pub(crate) trait AnswerLookup {
    /// The line that answers `key` (see [`LineClass::answers`]), if indexed.
    fn answer(&mut self, key: &str) -> Option<SourceLine>;
}

/// The result of projecting a page.
#[derive(Debug, Default)]
pub(crate) struct Projection {
    pub items: Vec<TimelineItem>,
    /// Lines that classified as visible but could not be projected.
    pub malformed: u64,
}

/// One terminal agent's transcript format.
pub(crate) trait TranscriptReader: Send + Sync {
    /// Herdr's `agent_session.agent` value, and the `{agent}` route segment.
    fn agent(&self) -> &'static str;
    /// The directory every transcript of this agent lives under. Nothing
    /// outside it is ever opened.
    fn root(&self) -> Option<PathBuf>;
    /// Whether `id` has the shape of this agent's session ids. Checked before
    /// any file system access.
    fn valid_session_id(&self, id: &str) -> bool;
    /// Propose the transcript file for a session. `root` is canonical; the
    /// proposal still passes the fence before it is opened.
    fn locate(&self, root: &Path, session_id: &str, hints: &LocateHints) -> Option<PathBuf>;
    /// Classify one complete line, without its newline. `None` means the line
    /// is malformed and is skipped and counted.
    fn classify(&self, line: &[u8]) -> Option<LineClass>;
    /// Project a page of visible lines (oldest first) into timeline items.
    fn project(&self, lines: &[SourceLine], lookup: &mut dyn AnswerLookup) -> Projection;
}

/// Every reader this build ships, by agent.
pub(crate) static READERS: &[&dyn TranscriptReader] = &[&claude::ClaudeReader];

/// The agents whose transcripts this gateway serves: the `agent_transcripts`
/// list in `/health` and `/api/discovery`.
pub(crate) fn supported_agents() -> Vec<&'static str> {
    READERS.iter().map(|reader| reader.agent()).collect()
}

pub(crate) fn reader_for(agent: &str) -> Option<&'static dyn TranscriptReader> {
    READERS
        .iter()
        .copied()
        .find(|reader| reader.agent() == agent)
}

/// The message id for a group opened at `line`. Zero-padded so ids sort in
/// file order, which is how the App orders timeline messages.
pub(crate) fn line_message_id(agent: &str, line: u64) -> String {
    format!("tx_{agent}_{line:012}")
}

/// Where sessions were last found, so a session found by scanning is not
/// scanned for again. Every hit is re-admitted through the fence.
static LOCATED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(&'static str, String), PathBuf>>,
> = std::sync::LazyLock::new(Default::default);
const MAX_LOCATED: usize = 512;

/// The fenced transcript file for a session under the canonical `root`.
fn locate(
    reader: &dyn TranscriptReader,
    root: &Path,
    session_id: &str,
    hints: &LocateHints,
) -> Option<PathBuf> {
    let key = (reader.agent(), session_id.to_owned());
    let remembered = LOCATED
        .lock()
        .ok()
        .and_then(|located| located.get(&key).cloned());
    if let Some(path) = remembered.and_then(|path| fence::admit(root, &path)) {
        return Some(path);
    }
    let path = fence::admit(root, &reader.locate(root, session_id, hints)?)?;
    if let Ok(mut located) = LOCATED.lock() {
        if located.len() >= MAX_LOCATED {
            located.clear();
        }
        located.insert(key, path.clone());
    }
    Some(path)
}

/// A reader's root, which a test may point at a fixture directory for the
/// current thread.
fn root_of(reader: &dyn TranscriptReader) -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(root) = test_roots::get(reader.agent()) {
        return Some(root);
    }
    reader.root()
}

#[cfg(test)]
pub(crate) mod test_roots {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;

    thread_local! {
        static ROOTS: RefCell<HashMap<&'static str, PathBuf>> = RefCell::new(HashMap::new());
    }

    pub(crate) fn set(agent: &'static str, root: PathBuf) {
        ROOTS.with(|roots| roots.borrow_mut().insert(agent, root));
    }

    pub(super) fn get(agent: &str) -> Option<PathBuf> {
        ROOTS.with(|roots| roots.borrow().get(agent).cloned())
    }
}
