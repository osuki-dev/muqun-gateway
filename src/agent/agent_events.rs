//! What the agents did while nobody was looking.
//!
//! The gateway already watches every agent's status so it can raise a push. A
//! push is a doorbell, though: it is delivered once, it may be silenced, and a
//! phone that was off the network for an hour comes back to a list of banners
//! that says nothing about the order things happened in or what is still
//! running. This is the other half -- a short, in-memory account of the status
//! transitions themselves, so that returning to a server after a while can be
//! answered with "here is what happened" rather than with the current state and
//! a shrug.
//!
//! # Deliberately forgetful
//!
//! Memory only, [`RING_CAPACITY`] transitions per session, oldest dropped. A
//! restarted gateway has nothing, and that is the honest answer: this describes
//! a session that has been running, and a gateway that has not been running was
//! not watching. Nothing here is written to disk, which is also what keeps it
//! out of the question of what a device may read after being revoked.
//!
//! # What a transition carries, and what it must not
//!
//! Pane id, the agent's own name, the status it left and the status it reached,
//! and when. Never terminal output, never a prompt, never the agent's own
//! wording -- the same rule the pushes are held to, for the same reason: this
//! is read on a lock screen's worth of attention by someone who has not decided
//! yet whether to open the app.
//!
//! # `since` and what a client can trust
//!
//! Sequence numbers are per session and strictly increasing, so a client polls
//! with the highest `seq` it has seen and gets exactly what is new. When the
//! ring has rolled past that point the answer says so with `missed`, because
//! "nothing happened" and "more happened than I keep" are different answers and
//! a digest that confuses them is worse than no digest.

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Value};

/// Transitions kept per session. A session that changed status two hundred
/// times has been busy for hours; a digest of more than that is a log file, and
/// a phone is not where a log file is read.
pub const RING_CAPACITY: usize = 200;

/// One agent status change, as the ring remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    /// Per-session, strictly increasing, and never reused. This is what `since`
    /// is expressed in.
    pub seq: u64,
    pub pane_id: String,
    /// The agent's own name, when Herdr reported one.
    pub agent: Option<String>,
    /// The status this pane was in before, absent when it is the first thing
    /// the gateway ever saw this pane do.
    pub from: Option<String>,
    pub to: String,
    pub unix_ms: u128,
}

impl AgentEvent {
    pub fn to_json(&self) -> Value {
        json!({
            "seq": self.seq,
            "pane_id": self.pane_id,
            "agent": self.agent,
            "from": self.from,
            "to": self.to,
            "unix_ms": self.unix_ms as u64,
        })
    }
}

/// One session's ring and its own sequence counter.
#[derive(Debug, Default)]
struct SessionRing {
    events: VecDeque<AgentEvent>,
    next_seq: u64,
}

/// Every session's recent transitions.
#[derive(Debug, Default)]
pub struct AgentEventLog {
    sessions: HashMap<String, SessionRing>,
}

impl AgentEventLog {
    /// Append one transition and hand back the event as recorded, sequence
    /// number and all. The caller has already decided this is a real change:
    /// the ring records what it is given rather than second-guessing it, so
    /// that the same transition can never be counted once for a push and twice
    /// here.
    pub fn record(
        &mut self,
        session_id: &str,
        pane_id: &str,
        agent: Option<&str>,
        from: Option<&str>,
        to: &str,
        unix_ms: u128,
    ) -> AgentEvent {
        let ring = self.sessions.entry(session_id.to_owned()).or_default();
        ring.next_seq += 1;
        let event = AgentEvent {
            seq: ring.next_seq,
            pane_id: pane_id.to_owned(),
            agent: agent.map(str::to_owned),
            from: from.map(str::to_owned),
            to: to.to_owned(),
            unix_ms,
        };
        ring.events.push_back(event.clone());
        while ring.events.len() > RING_CAPACITY {
            ring.events.pop_front();
        }
        event
    }

    /// Everything this session recorded after `since`, oldest first, which is
    /// the order a digest reads in. `None` means everything still held.
    pub fn since(&self, session_id: &str, since: Option<u64>) -> Vec<AgentEvent> {
        let Some(ring) = self.sessions.get(session_id) else {
            return Vec::new();
        };
        ring.events
            .iter()
            .filter(|event| since.is_none_or(|since| event.seq > since))
            .cloned()
            .collect()
    }

    /// The highest sequence this session has issued, which is what a client
    /// polls with next. Zero for a session that has done nothing, so a first
    /// call with no `since` and a later call with `since=0` mean the same.
    pub fn latest_seq(&self, session_id: &str) -> u64 {
        self.sessions
            .get(session_id)
            .map_or(0, |ring| ring.next_seq)
    }

    /// Whether the ring has already dropped something the caller asked for.
    ///
    /// True only when `since` names a point the ring no longer reaches back to.
    /// A client that gets this knows its digest is a sample rather than an
    /// account, which is a thing worth saying out loud rather than papering
    /// over with a shorter list.
    pub fn missed(&self, session_id: &str, since: Option<u64>) -> bool {
        let (Some(ring), Some(since)) = (self.sessions.get(session_id), since) else {
            return false;
        };
        match ring.events.front() {
            // Everything after `since` is still here when the oldest retained
            // event is the very next one, or an earlier one.
            Some(oldest) => oldest.seq > since + 1,
            // An empty ring for a session that has issued sequences means every
            // one of them rolled off.
            None => ring.next_seq > since,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_with(session: &str, count: u64) -> AgentEventLog {
        let mut log = AgentEventLog::default();
        for index in 0..count {
            log.record(
                session,
                "w1:p1",
                Some("claude"),
                Some("idle"),
                "working",
                1_000 + u128::from(index),
            );
        }
        log
    }

    #[test]
    fn a_session_remembers_its_last_transitions_and_forgets_the_rest() {
        let log = log_with("default", RING_CAPACITY as u64 + 50);
        let held = log.since("default", None);

        assert_eq!(held.len(), RING_CAPACITY);
        // The newest are what survived, and the sequence numbers of the dropped
        // ones are not reused.
        assert_eq!(held.first().unwrap().seq, 51);
        assert_eq!(held.last().unwrap().seq, RING_CAPACITY as u64 + 50);
        assert_eq!(log.latest_seq("default"), RING_CAPACITY as u64 + 50);
    }

    #[test]
    fn since_answers_only_what_is_new_and_says_when_it_cannot() {
        let mut log = log_with("default", 3);

        assert_eq!(log.since("default", Some(3)), Vec::new());
        assert_eq!(
            log.since("default", Some(1))
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        // Nothing was dropped, so no client is being told it missed anything.
        assert!(!log.missed("default", Some(1)));
        assert!(!log.missed("default", Some(0)));
        assert!(!log.missed("default", None));

        // Roll the ring past where that client was reading.
        for _ in 0..RING_CAPACITY {
            log.record("default", "w1:p1", None, Some("working"), "idle", 2_000);
        }
        assert!(log.missed("default", Some(1)));
        assert!(!log.missed("default", Some(log.latest_seq("default"))));

        // A session nobody watched has nothing and has missed nothing.
        assert_eq!(log.since("other", None), Vec::new());
        assert_eq!(log.latest_seq("other"), 0);
        assert!(!log.missed("other", Some(5)));
    }

    #[test]
    fn sessions_keep_their_own_sequence_and_never_read_each_others() {
        let mut log = log_with("left", 2);
        log.record("right", "w9:p9", Some("codex"), None, "blocked", 5_000);

        assert_eq!(log.latest_seq("left"), 2);
        assert_eq!(log.latest_seq("right"), 1);
        let right = log.since("right", None);
        assert_eq!(right.len(), 1);
        assert_eq!(right[0].pane_id, "w9:p9");
        assert_eq!(right[0].from, None);
        assert_eq!(right[0].to, "blocked");
        assert_eq!(right[0].to_json()["agent"], json!("codex"));
    }

    #[test]
    fn an_event_carries_ids_and_statuses_and_nothing_the_agent_wrote() {
        let mut log = AgentEventLog::default();
        let event = log.record(
            "default",
            "w1:p1",
            Some("claude"),
            Some("working"),
            "blocked",
            1_700,
        );
        let value = event.to_json();

        assert_eq!(
            value,
            json!({
                "seq": 1,
                "pane_id": "w1:p1",
                "agent": "claude",
                "from": "working",
                "to": "blocked",
                "unix_ms": 1_700
            })
        );
    }
}
