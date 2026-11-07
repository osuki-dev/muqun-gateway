//! Which agent events earn a system notification, and what it says.
//!
//! The rules and the words are the gateway's native agent pushes
//! (`terminal/routes.rs` `AgentPushGates`, `connectivity/push.rs`), so the
//! desktop says what the phone says:
//!
//! * a pending approval or question notifies once per request (T3 announces
//!   open requests again with every snapshot);
//! * a turn end is a busy-to-idle change that stayed idle for
//!   [`COMPLETION_GRACE`] (an agent pausing between steps is not finished),
//!   at most once per [`COMPLETION_COOLDOWN`] per session, and never for a
//!   child session -- its parent's turn is what the owner waits on;
//! * the title is the session's title when it has a real one, else the
//!   heading ("Approval needed"); the body names the agent and what happened,
//!   never the agent's own words.
//!
//! The core runs [`watch`] next to the projection; the UI thread decides
//! whether the owner is looking at that session ([`compose`] is pure).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use muqun_gateway::agents::{AgentDomainEvent, AgentSessionInfo, AgentSessionStatus};
use muqun_gateway::platform::i18n;
use tokio::sync::broadcast;

use crate::config::Language;
use crate::hub::SessionHub;
use crate::model::inbox::{Inbox, Update};
use crate::platform::notify::{self, Notice};

/// Idle this long after a turn before it counts as finished (gateway
/// `COMPLETION_GRACE`).
pub const COMPLETION_GRACE: Duration = Duration::from_secs(30);
/// No second "finished" for a session within this (gateway `COMPLETION_COOLDOWN`).
pub const COMPLETION_COOLDOWN: Duration = Duration::from_secs(120);
/// Forget everything past this many open requests or sessions; costs at most
/// a repeated or a missed notification.
const CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionKind {
    Approval,
    Question,
    Finished,
}

/// A session wants the owner (or is done).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    pub asid: String,
    pub kind: AttentionKind,
}

/// The gateway's `AgentPushGates`, keyed the same way.
#[derive(Debug, Default)]
pub struct Gate {
    approvals: HashSet<(String, String)>,
    forms: HashSet<(String, String)>,
    statuses: HashMap<String, AgentSessionStatus>,
    idle_since: HashMap<String, Instant>,
    completed_at: HashMap<String, Instant>,
}

impl Gate {
    /// What `event` raises now, if anything.
    pub fn admit(&mut self, event: &AgentDomainEvent, now: Instant) -> Option<Attention> {
        use AgentDomainEvent as E;
        let raise = |asid: &str, kind| {
            Some(Attention {
                asid: asid.to_string(),
                kind,
            })
        };
        match event {
            E::PermissionPending { asid, request, .. } => {
                if first_sighting(&mut self.approvals, &asid.0, &request.id) {
                    return raise(&asid.0, AttentionKind::Approval);
                }
            }
            E::FormPending { asid, request, .. } => {
                if first_sighting(&mut self.forms, &asid.0, &request.id) {
                    return raise(&asid.0, AttentionKind::Question);
                }
            }
            E::PermissionResolved {
                asid, request_id, ..
            } => {
                self.approvals.remove(&(asid.0.clone(), request_id.clone()));
            }
            E::FormResolved { asid, form_id, .. } => {
                self.forms.remove(&(asid.0.clone(), form_id.clone()));
            }
            E::StatusChanged { asid, status, .. } => self.observe(&asid.0, *status, now),
            _ => {}
        }
        None
    }

    fn observe(&mut self, asid: &str, status: AgentSessionStatus, now: Instant) {
        use AgentSessionStatus as S;
        if self.statuses.len() >= CAPACITY {
            self.statuses.clear();
        }
        let previous = self.statuses.insert(asid.to_string(), status);
        match status {
            S::Idle if matches!(previous, Some(S::Busy | S::Retry)) => {
                self.idle_since.insert(asid.to_string(), now);
            }
            S::Idle => {}
            _ => {
                self.idle_since.remove(asid);
            }
        }
    }

    /// Sessions whose turn end is confirmed by `now`; each is handed out once.
    pub fn finished(&mut self, now: Instant) -> Vec<String> {
        let ready: Vec<String> = self
            .idle_since
            .iter()
            .filter(|(_, since)| now.saturating_duration_since(**since) >= COMPLETION_GRACE)
            .map(|(asid, _)| asid.clone())
            .collect();
        self.completed_at
            .retain(|_, sent| now.saturating_duration_since(*sent) < COMPLETION_COOLDOWN);
        let mut confirmed = Vec::new();
        for asid in ready {
            self.idle_since.remove(&asid);
            if !self.completed_at.contains_key(&asid) {
                self.completed_at.insert(asid.clone(), now);
                confirmed.push(asid);
            }
        }
        confirmed
    }
}

fn first_sighting(open: &mut HashSet<(String, String)>, asid: &str, id: &str) -> bool {
    if open.len() >= CAPACITY {
        open.clear();
    }
    open.insert((asid.to_string(), id.to_string()))
}

/// Feed every agent event through a [`Gate`] and hand what it raises to the
/// UI as [`Update::Attention`]. An answered request takes its notification
/// down. Runs for the life of the core.
pub async fn watch(hub: Arc<dyn SessionHub>, inbox: Arc<Inbox>) {
    let mut events = hub.events();
    let mut gate = Gate::default();
    // The turn-end grace is not marked by any event: look every two seconds,
    // the gateway's own beat.
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            next = events.recv() => match next {
                Ok(event) => {
                    match &event {
                        AgentDomainEvent::PermissionResolved { asid, .. }
                        | AgentDomainEvent::FormResolved { asid, .. } => notify::withdraw(&asid.0),
                        _ => {}
                    }
                    if let Some(attention) = gate.admit(&event, Instant::now()) {
                        inbox.push(Update::Attention(attention));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                for asid in gate.finished(Instant::now()) {
                    inbox.push(Update::Attention(Attention { asid, kind: AttentionKind::Finished }));
                }
            }
        }
    }
}

/// Visible characters of a session title a notification shows.
const MAX_TITLE_CHARS: usize = 40;

/// A session's title as a notification title, or `None` when it has no real
/// one (gateway `push_session_title`): empty, the session id, the agent's own
/// name, a stock "new session" label or a path. Whitespace runs become one
/// space, control characters go, and it is cut at 40 graphemes with "…".
pub fn session_title(raw: &str, asid: &str, agent_id: &str) -> Option<String> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut clean = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !clean.is_empty() && !clean.ends_with(' ') {
                clean.push(' ');
            }
        } else if !ch.is_control() {
            clean.push(ch);
        }
    }
    let clean = clean.trim_end();
    let lower = clean.to_lowercase();
    let is_id = clean == asid
        || (clean.len() > 4
            && clean.starts_with("ses_")
            && clean[4..].chars().all(|c| c.is_ascii_alphanumeric()));
    let is_path = clean.starts_with(['/', '~', '\\'])
        || clean
            .get(1..3)
            .is_some_and(|rest| rest == ":\\" || rest == ":/");
    let is_stock = matches!(
        lower.as_str(),
        "new session"
            | "new chat"
            | "new conversation"
            | "untitled"
            | "untitled session"
            | "session"
    ) || muqun_gateway::agents::runtime::agent_display_name(agent_id)
        .is_some_and(|name| name.to_lowercase() == lower);
    if clean.is_empty() || is_id || is_path || is_stock {
        return None;
    }
    let mut graphemes = clean.graphemes(true);
    let mut title: String = graphemes.by_ref().take(MAX_TITLE_CHARS).collect();
    if graphemes.next().is_some() {
        title.push('…');
    }
    Some(title)
}

/// The notification for `attention`, or `None` when it should not be raised
/// (a child session's turn end). `info` is the session as listed, when it is;
/// `agent` the agent's name as the UI shows it.
pub fn compose(
    attention: &Attention,
    info: Option<&AgentSessionInfo>,
    agent: Option<&str>,
    language: Language,
) -> Option<Notice> {
    if attention.kind == AttentionKind::Finished && info.is_some_and(|i| i.parent_id.is_some()) {
        return None;
    }
    let locale = language.locale();
    let (heading, body) = match attention.kind {
        AttentionKind::Approval => ("Approval needed", "{name} is waiting for your approval."),
        AttentionKind::Question => ("Agent blocked", "{name} needs your input."),
        AttentionKind::Finished => ("Agent done", "{name} finished running."),
    };
    let agent_id = info.map_or("", |i| i.agent_id.as_str());
    let name = agent
        .filter(|n| !n.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| muqun_gateway::agents::runtime::agent_display_name(agent_id).map(str::to_owned))
        .unwrap_or_else(|| i18n::t(locale, "Agent").to_owned());
    let title = info
        .and_then(|i| session_title(&i.title, &attention.asid, agent_id))
        .unwrap_or_else(|| i18n::t(locale, heading).to_owned());
    Some(Notice {
        key: attention.asid.clone(),
        title,
        body: i18n::t_slots(locale, body, &[("name", &name)]),
        waiting: attention.kind != AttentionKind::Finished,
        action: crate::ui::copy::tr(language, "Open", "打开"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::AgentSessionId;

    fn status(asid: &str, status: AgentSessionStatus) -> AgentDomainEvent {
        AgentDomainEvent::StatusChanged {
            asid: AgentSessionId(asid.into()),
            status,
            error: None,
            seq: 0,
        }
    }

    /// The gateway's turn-end rule: busy then idle, confirmed after the
    /// grace, at most once per cooldown; working again cancels it.
    #[test]
    fn a_turn_end_is_raised_after_the_grace_and_once_per_cooldown() {
        use AgentSessionStatus::{Busy, Idle};
        let mut gate = Gate::default();
        let t0 = Instant::now();
        assert_eq!(gate.admit(&status("a", Idle), t0), None);
        gate.admit(&status("a", Busy), t0);
        gate.admit(&status("a", Idle), t0);
        assert!(gate.finished(t0 + Duration::from_secs(29)).is_empty());
        assert_eq!(gate.finished(t0 + COMPLETION_GRACE), vec!["a".to_string()]);
        assert!(gate.finished(t0 + Duration::from_secs(40)).is_empty());

        // A second turn inside the cooldown is not announced again.
        let t1 = t0 + Duration::from_secs(60);
        gate.admit(&status("a", Busy), t1);
        gate.admit(&status("a", Idle), t1);
        assert!(gate.finished(t1 + COMPLETION_GRACE).is_empty());

        // Idle, then busy again within the grace: not finished.
        let t2 = t0 + Duration::from_secs(600);
        gate.admit(&status("b", Busy), t2);
        gate.admit(&status("b", Idle), t2);
        gate.admit(&status("b", Busy), t2 + Duration::from_secs(5));
        assert!(gate.finished(t2 + COMPLETION_GRACE * 2).is_empty());
    }

    #[test]
    fn titles_follow_the_gateway_push_rule() {
        let long = "a".repeat(60);
        let cut = format!("{}…", "a".repeat(40));
        let cases: [(&str, Option<&str>); 7] = [
            ("  Fix\n the\tlogin bug ", Some("Fix the login bug")),
            (long.as_str(), Some(cut.as_str())),
            ("", None),
            ("New session", None),
            ("Codex", None),
            ("ses_abc123", None),
            ("/home/someone/project", None),
        ];
        for (raw, want) in cases {
            assert_eq!(
                session_title(raw, "ses_1", "codex").as_deref(),
                want,
                "{raw:?}"
            );
        }
    }
}
