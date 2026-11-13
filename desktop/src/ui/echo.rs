//! The owner's message, shown the moment it is sent.
//!
//! A prompt sent from the session composer (Enter; while the agent works it
//! steers) appears at once at the foot of the timeline as the owner's bubble,
//! its pictures above it, saying "Sending…" until the gateway takes it. The
//! gateway's own user row replaces it when it arrives: the first new user row
//! that says the same words claims the echo, so the message is never drawn
//! twice. A steer the gateway holds until the agent's next step shows up in
//! the session's inbox; the echo claims that item too (the composer's queue
//! strip leaves it out) and says it is waiting for the agent. A send that
//! fails stays as a failed bubble with Retry and Discard.
//!
//! Queued sends (Alt+Enter) draw no echo: their inbox item is the queued row,
//! in the queue strip as before. Commands and skills draw none either.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use muqun_gateway::agents::ports::agent::prompt_display_body;
use muqun_gateway::agents::{AgentPart, AgentSessionId, TimelineItem, TimelineRole};
use rune::{col, row, text, El};
use rune_kit::{Button, ButtonVariant, ControlSize, Spinner};

use super::parts::Tcx;
use super::UiState;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, InboxItem, Target};

/// A delivered echo whose row never came (an agent that rewrites the words,
/// a missed event) leaves once the session has been idle this long.
const IDLE_TTL: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoState {
    /// On its way to the gateway.
    Sending,
    /// The gateway took it; its row has not arrived yet.
    Sent,
    /// The gateway refused it, or could not be reached.
    Failed,
}

#[derive(Debug, Clone)]
pub struct Echo {
    /// The submit's token: which `Submitted` answers it, and the row's key.
    pub token: u64,
    pub text: String,
    /// The staged files sent with it (host paths).
    pub files: Vec<String>,
    /// Which of them are pictures.
    pub images: Vec<String>,
    /// Sent while the agent was working: it steers the running turn.
    pub steer: bool,
    pub state: EchoState,
    /// User rows that already said these words: none of them is this one.
    known: HashSet<String>,
    /// The inbox item the gateway holds it as, until the agent takes it.
    pub inbox: Option<String>,
    /// When it last changed state.
    pub at: Instant,
}

/// What a user row says, as the echo compares it: the words before any
/// "Attached files" list, trimmed.
fn words(text: &str) -> &str {
    prompt_display_body(text).trim()
}

fn user_words(item: &TimelineItem) -> Option<&str> {
    match (&item.role, &item.part) {
        (TimelineRole::User, AgentPart::Text { text }) => Some(words(text)),
        _ => None,
    }
}

impl Echo {
    fn new(
        token: u64,
        text: String,
        files: Vec<String>,
        images: Vec<String>,
        steer: bool,
        rows: &[TimelineItem],
    ) -> Self {
        let said = words(&text).to_string();
        let known = rows
            .iter()
            .filter(|r| user_words(r) == Some(said.as_str()))
            .map(|r| r.id.clone())
            .collect();
        Self {
            token,
            text,
            files,
            images,
            steer,
            state: EchoState::Sending,
            known,
            inbox: None,
            at: Instant::now(),
        }
    }

    fn says(&self, words_: &str) -> bool {
        words(&self.text) == words_
    }
}

/// Drop each echo a new user row in `rows` stands for, one row per echo.
/// Returns whether any went.
pub fn reconcile(echoes: &mut Vec<Echo>, rows: &mut [TimelineItem]) -> bool {
    let before = echoes.len();
    let len = rows.len();
    let start = len.saturating_sub(200);
    let recent = &mut rows[start..];
    for row in recent {
        let Some(said) = user_words(row).map(|s| s.to_string()) else {
            continue;
        };
        let Some(at) = echoes.iter().position(|e| {
            e.state != EchoState::Failed && e.says(&said) && !e.known.contains(&row.id)
        }) else {
            continue;
        };
        let echo = echoes.remove(at);
        if row.attachments.as_ref().is_none_or(|a| a.is_empty()) && !echo.images.is_empty() {
            row.attachments = Some(echo.images.clone());
        }
        // The row is spoken for: a second echo of the same words waits for
        // a row of its own.
        for e in echoes.iter_mut().filter(|e| e.says(&said)) {
            e.known.insert(row.id.clone());
        }
    }
    echoes.len() != before
}

/// The session's inbox changed: a steer item with an echo's words is that
/// echo (until the agent takes it); an item gone from the inbox lets go.
/// Returns whether anything changed.
pub fn claim_inbox(echoes: &mut [Echo], items: &[InboxItem]) -> bool {
    let mut changed = false;
    for e in echoes.iter_mut() {
        if let Some(id) = &e.inbox {
            if !items.iter().any(|i| &i.id == id) {
                e.inbox = None;
                e.at = Instant::now();
                changed = true;
            }
        }
    }
    for item in items.iter().filter(|i| i.steer) {
        if echoes.iter().any(|e| e.inbox.as_ref() == Some(&item.id)) {
            continue;
        }
        if let Some(e) = echoes.iter_mut().find(|e| {
            e.inbox.is_none()
                && e.state != EchoState::Failed
                && e.files.len() == item.files.len()
                && e.says(words(&item.text))
        }) {
            e.inbox = Some(item.id.clone());
            changed = true;
        }
    }
    changed
}

impl UiState {
    /// A prompt is being sent to `asid`: show it now.
    pub fn echo_sent(
        &mut self,
        asid: &str,
        token: u64,
        text: String,
        staged: &[crate::model::compose::StagedFile],
        steer: bool,
    ) {
        let rows = self
            .timelines
            .get(asid)
            .map(|t| t.items.as_slice())
            .unwrap_or_default();
        let files = staged.iter().map(|f| f.path.clone()).collect();
        let images = staged
            .iter()
            .filter(|f| f.image)
            .map(|f| f.path.clone())
            .collect();
        let echo = Echo::new(token, text, files, images, steer, rows);
        self.echoes.entry(asid.to_string()).or_default().push(echo);
        self.echo_tokens.insert(token);
        self.echo_rev += 1;
    }

    /// The gateway answered submit `token`. `false` when no echo waits on it.
    pub fn echo_submitted(&mut self, token: u64, ok: bool) -> bool {
        if !self.echo_tokens.remove(&token) {
            return false;
        }
        if let Some(echo) = self
            .echoes
            .values_mut()
            .flat_map(|v| v.iter_mut())
            .find(|e| e.token == token)
        {
            echo.state = if ok {
                EchoState::Sent
            } else {
                EchoState::Failed
            };
            echo.at = Instant::now();
            self.echo_rev += 1;
        }
        true
    }

    /// `asid`'s timeline changed: drop the echoes its new rows stand for.
    pub fn echo_rows(&mut self, asid: &str) {
        let (Some(echoes), Some(tl)) = (self.echoes.get_mut(asid), self.timelines.get_mut(asid))
        else {
            return;
        };
        if reconcile(echoes, &mut tl.items) {
            self.echo_rev += 1;
        }
    }

    /// `asid`'s inbox is now `items`.
    pub fn echo_inbox(&mut self, asid: &str, items: &[InboxItem]) {
        if let Some(echoes) = self.echoes.get_mut(asid) {
            if claim_inbox(echoes, items) {
                self.echo_rev += 1;
            }
        }
    }

    /// Whether inbox item `id` of the open session is drawn as an echo.
    pub fn echoed(&self, id: &str) -> bool {
        self.selected
            .as_deref()
            .and_then(|asid| self.echoes.get(asid))
            .is_some_and(|v| v.iter().any(|e| e.inbox.as_deref() == Some(id)))
    }

    pub fn echoes_of(&self, asid: &str) -> &[Echo] {
        self.echoes.get(asid).map_or(&[], Vec::as_slice)
    }

    /// Send a failed echo again, as it was.
    fn retry_echo(&mut self, asid: &str, token: u64) {
        let busy = self.session_busy(asid);
        let next = self.token(super::Slot::Session);
        let Some(echo) = self
            .echoes
            .get_mut(asid)
            .and_then(|v| v.iter_mut().find(|e| e.token == token))
        else {
            return;
        };
        echo.token = next;
        echo.state = EchoState::Sending;
        self.echo_tokens.insert(next);
        echo.steer = busy;
        echo.at = Instant::now();
        let (text, files) = (echo.text.clone(), echo.files.clone());
        self.echo_rev += 1;
        self.send(Command::Compose(ComposeCommand::Submit {
            token: next,
            target: Target::Session(AgentSessionId(asid.to_string())),
            deliver: Deliver::Prompt(text),
            attachments: files,
            delivery: busy.then(|| "steer".to_string()),
        }));
    }

    fn discard_echo(&mut self, asid: &str, token: u64) {
        if let Some(v) = self.echoes.get_mut(asid) {
            v.retain(|e| e.token != token);
            self.echo_rev += 1;
        }
    }

    /// Pull a steered echo back into the composer draft for editing before the agent reads it.
    pub fn edit_steered_echo(&mut self, asid: &str, token: u64, inbox_id: &str, text: String) {
        if !inbox_id.is_empty() {
            self.send(Command::Compose(ComposeCommand::InboxCancel {
                asid: AgentSessionId(asid.to_string()),
                item: inbox_id.to_string(),
            }));
            self.queue.retain(|q| q.id != inbox_id);
        }
        self.discard_echo(asid, token);
        let len = text.len();
        self.draft = super::composer::fresh(text);
        self.draft.set_caret(len);
        self.persist_draft(super::Slot::Session);
    }

    /// Cancel a steered echo before the agent reads it.
    pub fn cancel_steered_echo(&mut self, asid: &str, token: u64, inbox_id: &str) {
        if !inbox_id.is_empty() {
            self.send(Command::Compose(ComposeCommand::InboxCancel {
                asid: AgentSessionId(asid.to_string()),
                item: inbox_id.to_string(),
            }));
            self.queue.retain(|q| q.id != inbox_id);
        }
        self.discard_echo(asid, token);
    }

    /// Before each frame: let go of delivered echoes whose row never came.
    pub fn poll_echoes(&mut self, tx: &mut rune::Tx) -> bool {
        let busy: HashSet<String> = self
            .echoes
            .keys()
            .filter(|asid| self.session_busy(asid))
            .cloned()
            .collect();
        let mut changed = false;
        for (asid, v) in self.echoes.iter_mut() {
            let before = v.len();
            v.retain(|e| {
                let stale = e.state == EchoState::Sent
                    && e.inbox.is_none()
                    && !busy.contains(asid)
                    && e.at.elapsed() >= IDLE_TTL;
                if !stale && e.state == EchoState::Sent && e.inbox.is_none() {
                    tx.wake_after(IDLE_TTL.saturating_sub(e.at.elapsed()));
                }
                !stale
            });
            changed |= v.len() != before;
        }
        self.echoes.retain(|_, v| !v.is_empty());
        if changed {
            self.echo_rev += 1;
        }
        changed
    }
}

/// An echo's row: the bubble as the gateway's row will draw it, a little
/// faded while it is on its way, and a line under it saying where it is.
pub fn view(cx: &Tcx, echo: &Echo) -> El {
    let k = cx.k;
    let key = format!("echo-{}", echo.token);
    let mut wrap = col()
        .key(key.clone())
        .w_full()
        .gap(4.0)
        .pt(10.0)
        .pb(4.0)
        .items_end();
    let paths: Vec<PathBuf> = echo.images.iter().map(PathBuf::from).collect();
    if let Some(g) = super::parts::images::gallery_sized(cx, &key, &paths, 0, 150.0) {
        wrap = wrap.child(g.justify_end().key("echo-imgs"));
    }
    let body = words(&echo.text).to_string();
    if !body.is_empty() {
        let bubble = col()
            .key("user")
            .max_w(620.0)
            .min_w(0.0)
            .shrink()
            .px(14.0)
            .py(8.0)
            .radius(k.radius.lg)
            .fill(k.c.layer_02)
            .child(super::parts::message::rich_text(cx, &key, &body));
        let bubble = match echo.state {
            EchoState::Sending => bubble.opacity(0.7),
            EchoState::Failed => bubble.border(1.0, k.c.danger.border),
            EchoState::Sent => bubble,
        };
        wrap = wrap.child(row().key("user-line").w_full().justify_end().child(bubble));
    }
    let note = |words: &str, color| text(words.to_string()).font(k.ty.caption()).color(color);
    let status = match echo.state {
        EchoState::Sending => Some(
            row()
                .gap(6.0)
                .center_y()
                .child(Spinner::new().size(10.0).build(k).key("echo-spin"))
                .child(note(cx.t("Sending…"), k.c.text_faint)),
        ),
        EchoState::Sent if echo.inbox.is_some() => {
            let asid = cx.asid.to_string();
            let token = echo.token;
            let inbox_id = echo.inbox.clone().unwrap_or_default();
            let text = echo.text.clone();
            let mut status_row = row().gap(6.0).center_y().child(note(
                cx.t("Steered · the agent reads it at its next step"),
                k.c.text_faint,
            ));
            if echo.files.is_empty() {
                let edit_asid = asid.clone();
                let edit_inbox_id = inbox_id.clone();
                let edit_text = text;
                status_row = status_row.child(
                    Button::new(cx.t("Edit"))
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| {
                            s.edit_steered_echo(
                                &edit_asid,
                                token,
                                &edit_inbox_id,
                                edit_text.clone(),
                            );
                        })
                        .build(k)
                        .key(format!("echo-edit-{token}")),
                );
            }
            status_row = status_row.child(
                Button::new(cx.t("Cancel"))
                    .variant(ButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        s.cancel_steered_echo(&asid, token, &inbox_id);
                    })
                    .build(k)
                    .key(format!("echo-cancel-{token}")),
            );
            Some(status_row)
        }
        EchoState::Sent => None,
        EchoState::Failed => {
            let asid = cx.asid.to_string();
            let token = echo.token;
            let retry_asid = asid.clone();
            Some(
                row()
                    .gap(4.0)
                    .center_y()
                    .child(note(cx.t("Not sent"), k.c.danger.fg).mr(4.0))
                    .child(
                        Button::new(cx.t("Retry"))
                            .variant(ButtonVariant::Ghost)
                            .control_size(ControlSize::Small)
                            .on_press(move |s: &mut UiState| s.retry_echo(&retry_asid, token))
                            .build(k)
                            .key("echo-retry"),
                    )
                    .child(
                        Button::new(cx.t("Discard"))
                            .variant(ButtonVariant::GhostMuted)
                            .control_size(ControlSize::Small)
                            .on_press(move |s: &mut UiState| s.discard_echo(&asid, token))
                            .build(k)
                            .key("echo-discard"),
                    ),
            )
        }
    };
    if let Some(status) = status {
        wrap = wrap.child(status.key("echo-status").role("status"));
    }
    wrap
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str, text: &str) -> TimelineItem {
        TimelineItem {
            id: id.into(),
            message_id: format!("m-{id}"),
            role: TimelineRole::User,
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

    fn echo(token: u64, text: &str, rows: &[TimelineItem]) -> Echo {
        Echo::new(token, text.into(), vec![], vec![], true, rows)
    }

    fn steer(id: &str, text: &str) -> InboxItem {
        InboxItem {
            id: id.into(),
            text: text.into(),
            steer: true,
            files: vec![],
        }
    }

    #[test]
    fn the_gateway_row_replaces_the_echo_once_and_old_rows_never_do() {
        // The same words were said earlier in the session.
        let mut rows = vec![user("u0", "run the tests")];
        let mut echoes = vec![
            echo(1, "run the tests ", &rows),
            echo(2, "run the tests", &rows),
        ];
        // Re-reading what was already there (a snapshot) claims nothing.
        assert!(!reconcile(&mut echoes, &mut rows));
        // The first send's row arrives, its file list appended by the gateway.
        rows.push(user(
            "u1",
            "run the tests\n\nAttached files (on this host):\n- /up/a.png",
        ));
        assert!(reconcile(&mut echoes, &mut rows));
        assert_eq!(echoes.iter().map(|e| e.token).collect::<Vec<_>>(), [2]);
        // The same rows again (a later snapshot) leave the second echo be.
        assert!(!reconcile(&mut echoes, &mut rows));
        rows.push(user("u2", "run the tests"));
        assert!(reconcile(&mut echoes, &mut rows));
        assert!(echoes.is_empty());
    }

    #[test]
    fn a_send_echoes_at_once_and_a_late_ack_never_clears_the_next_draft() {
        use crate::model::compose::ComposerUpdate;
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.timelines.entry("s1".into()).or_default().replace(vec![]);
        let token = s.token(super::super::Slot::Session);
        s.echo_sent("s1", token, "ship it".into(), &[], false);
        assert_eq!(s.echoes_of("s1")[0].state, EchoState::Sending);
        // The gateway's row lands before the submit is acknowledged.
        s.timelines
            .get_mut("s1")
            .unwrap()
            .upsert(vec![user("u1", "ship it")]);
        s.echo_rows("s1");
        assert!(s.echoes_of("s1").is_empty());
        s.draft = rune::TextEdit::new("next thought");
        s.on_composer(ComposerUpdate::Submitted { token, ok: true });
        assert_eq!(s.draft.text, "next thought");
        assert!(s.pending_submit.is_none());
        // A refused send stays on screen, failed, for Retry.
        let token = s.token(super::super::Slot::Session);
        s.echo_sent("s1", token, "and lint".into(), &[], false);
        s.on_composer(ComposerUpdate::Submitted { token, ok: false });
        assert_eq!(s.echoes_of("s1")[0].state, EchoState::Failed);
        assert_eq!(s.draft.text, "next thought");
    }

    #[test]
    fn a_failed_echo_stays_and_a_held_steer_is_the_echo_until_taken() {
        let mut echoes = vec![echo(1, "stop and explain", &[]), echo(2, "also lint", &[])];
        echoes[1].state = EchoState::Failed;
        // The gateway holds the steer: the echo stands for its inbox item.
        let held = [steer("q1", "stop and explain"), steer("q2", "also lint")];
        assert!(claim_inbox(&mut echoes, &held));
        assert_eq!(echoes[0].inbox.as_deref(), Some("q1"));
        // A failed send is no inbox item, and no row takes it away.
        assert_eq!(echoes[1].inbox, None);
        assert!(!reconcile(&mut echoes, &mut [user("u9", "also lint")]));
        assert_eq!(echoes.len(), 2);
        // The agent took it: the item leaves the inbox, the echo waits for its row.
        assert!(claim_inbox(&mut echoes, &[]));
        assert_eq!(echoes[0].inbox, None);
        assert!(reconcile(
            &mut echoes,
            &mut [user("u3", "stop and explain")]
        ));
        assert_eq!(echoes.len(), 1);
        assert_eq!(echoes[0].state, EchoState::Failed);
    }

    #[test]
    fn edit_and_cancel_steered_echo_restores_draft_or_discards() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.selected = Some("s1".into());
        let token = 42;
        s.echo_sent("s1", token, "typo in my prompt".into(), &[], true);
        s.queue.push(steer("inbox-1", "typo in my prompt"));
        s.echo_inbox("s1", &s.queue.clone());
        assert_eq!(s.echoes_of("s1")[0].inbox.as_deref(), Some("inbox-1"));

        // User edits the steered echo
        s.edit_steered_echo("s1", token, "inbox-1", "typo in my prompt".into());

        // Echo is removed and draft is restored with the text
        assert!(s.echoes_of("s1").is_empty());
        assert_eq!(s.draft.text, "typo in my prompt");
        assert!(s.draft.focused);
        assert!(s.queue.is_empty());

        // Gateway received InboxCancel command
        let mut found = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(
                cmd,
                Command::Compose(ComposeCommand::InboxCancel { asid, item })
                    if asid.0 == "s1" && item == "inbox-1"
            ) {
                found = true;
                break;
            }
        }
        assert!(found);
    }
}
