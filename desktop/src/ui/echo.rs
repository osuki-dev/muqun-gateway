//! Idle prompts receive a local timeline echo until their canonical user row arrives.
//! A busy submission stays in flight until the gateway confirms it. An authoritative
//! inbox item owns its editable preview, and the provider owns the delivered row.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use muqun_gateway::agents::ports::agent::prompt_display_body;
use muqun_gateway::agents::{AgentPart, AgentSessionId, TimelineItem, TimelineRole};
use rune::{col, row, text, El};
use rune_kit::{Button, ButtonVariant, ControlSize};

use super::parts::Tcx;
use super::UiState;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Target};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyDelivery {
    Steer,
    Queue,
}

impl BusyDelivery {
    pub fn wire(self) -> String {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queue",
        }
        .to_owned()
    }
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
    /// Requested delivery while the agent was working.
    pub delivery: Option<BusyDelivery>,
    pub state: EchoState,
    /// User rows that already said these words: none of them is this one.
    known: HashSet<String>,
    inbox_id: Option<String>,
    /// When it last changed state.
    pub at: Instant,
}

/// What a user row says, as the echo compares it: the words before any
/// "Attached files" list or Gateway orchestration/task wrapper, trimmed.
fn words(text: &str) -> &str {
    let body = prompt_display_body(text).trim();
    let body = body
        .split_once("\n\nYou were started for a Muqun task:")
        .map_or(body, |(head, _)| head.trim());
    if body.starts_with("Implement and verify the user goal carefully.") {
        if let Some((_, goal)) = body.rsplit_once("\nUser goal:\n") {
            return goal.trim();
        }
    }
    if body.starts_with("Coordinate this task") {
        if let Some((_, goal)) = body.rsplit_once("\n\nGoal:\n") {
            return goal.trim();
        }
    }
    body
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
        delivery: Option<BusyDelivery>,
        rows: &[TimelineItem],
    ) -> Self {
        let said = words(&text).to_string();
        let known = rows
            .iter()
            .filter(|r| user_words(r) == Some(said.as_str()))
            .flat_map(|r| [r.id.clone(), r.message_id.clone()])
            .collect();
        Self {
            token,
            text,
            files,
            images,
            delivery,
            state: EchoState::Sending,
            known,
            inbox_id: None,
            at: Instant::now(),
        }
    }

    fn says(&self, words_: &str) -> bool {
        words(&self.text) == words_
    }
}

fn same_attachment(asid: &str, staged: &str, received: &str) -> bool {
    if staged == received {
        return true;
    }
    let Some(id) = received
        .strip_prefix("/api/agent-images/")
        .and_then(|rest| rest.strip_suffix("/original"))
    else {
        return false;
    };
    id == muqun_gateway::agents::message_images::original_asset_id(asid, staged)
}

/// Drop each echo a new user row in `rows` stands for, one row per echo.
/// Returns whether any went.
fn reconcile(
    asid: &str,
    echoes: &mut Vec<Echo>,
    rows: &mut [TimelineItem],
    sent_images: &mut HashMap<String, Vec<String>>,
) -> bool {
    let before = echoes.len();
    let len = rows.len();
    let start = len.saturating_sub(200);
    let recent = &mut rows[start..];
    for row in recent {
        let Some(said) = user_words(row).map(|s| s.to_string()) else {
            continue;
        };
        let Some(at) = echoes.iter().position(|e| {
            e.state != EchoState::Failed
                && e.inbox_id.as_ref().map_or_else(
                    || {
                        e.says(&said)
                            && row.attachments.as_ref().is_none_or(|files| {
                                files.is_empty()
                                    || e.files.is_empty()
                                    || (files.len() == e.files.len()
                                        && e.files.iter().zip(files).all(|(staged, received)| {
                                            same_attachment(asid, staged, received)
                                        }))
                            })
                    },
                    |id| id == &row.message_id,
                )
                && !e.known.contains(&row.id)
                && !e.known.contains(&row.message_id)
        }) else {
            continue;
        };
        let echo = echoes.remove(at);
        if row.attachments.as_ref().is_none_or(|a| a.is_empty()) && !echo.images.is_empty() {
            row.attachments = Some(echo.images.clone());
            sent_images.insert(row.id.clone(), echo.images);
        }
        // The row is spoken for: a second echo of the same words waits for
        // a row of its own.
        for e in echoes.iter_mut().filter(|e| e.says(&said)) {
            e.known.insert(row.id.clone());
            e.known.insert(row.message_id.clone());
        }
    }
    echoes.len() != before
}

impl UiState {
    /// A prompt is being sent to `asid`: show it now.
    pub fn echo_sent(
        &mut self,
        asid: &str,
        token: u64,
        text: String,
        staged: &[crate::model::compose::StagedFile],
        delivery: Option<BusyDelivery>,
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
        let mut echo = Echo::new(token, text, files, images, delivery, rows);
        if self.selected.as_deref() == Some(asid) {
            echo.known
                .extend(self.queue.iter().map(|item| item.id.clone()));
        }
        self.echoes.entry(asid.to_string()).or_default().push(echo);
        self.echo_tokens.insert(token);
        self.echo_rev += 1;
    }

    /// The gateway answered submit `token`. `false` when no echo waits on it.
    pub fn echo_submitted(&mut self, token: u64, ok: bool) -> bool {
        if !self.echo_tokens.remove(&token) {
            return false;
        }
        for echoes in self.echoes.values_mut() {
            if let Some(index) = echoes.iter().position(|e| e.token == token) {
                let echo = &mut echoes[index];
                echo.state = if ok {
                    EchoState::Sent
                } else {
                    EchoState::Failed
                };
                echo.at = Instant::now();
                self.echo_rev += 1;
                break;
            }
        }
        true
    }

    /// Hand the preview to the editable queue item as soon as it appears.
    /// Keep its native receipt ID to match the eventual timeline row.
    pub(super) fn echo_inbox(&mut self, asid: &str, items: &[crate::model::compose::InboxItem]) {
        let Some(echoes) = self.echoes.get_mut(asid) else {
            return;
        };
        for item in items {
            if echoes.iter().any(|e| e.inbox_id.as_ref() == Some(&item.id)) {
                continue;
            }
            let mode = if item.steer {
                BusyDelivery::Steer
            } else {
                BusyDelivery::Queue
            };
            if let Some(index) = echoes.iter().position(|e| {
                (e.delivery == Some(mode) || e.delivery.is_none())
                    && e.inbox_id.is_none()
                    && e.state != EchoState::Failed
                    && e.says(words(&item.text))
                    && !e.known.contains(&item.id)
                    && (item.files.is_empty()
                        || e.files
                            .iter()
                            .map(String::as_str)
                            .eq(item.files.iter().map(|f| f.path.as_str())))
            }) {
                echoes[index].delivery = Some(mode);
                echoes[index].inbox_id = Some(item.id.clone());
                for (other, echo) in echoes.iter_mut().enumerate() {
                    if other != index {
                        echo.known.insert(item.id.clone());
                    }
                }
                self.echo_rev += 1;
            }
        }
    }

    pub(super) fn queue_echo_images(&self, asid: &str, inbox_id: &str) -> &[String] {
        self.echoes_of(asid)
            .iter()
            .find(|echo| echo.inbox_id.as_deref() == Some(inbox_id))
            .map(|echo| echo.images.as_slice())
            .or_else(|| {
                let tl = self.timelines.get(asid)?;
                if tl.sent_images.is_empty() {
                    return None;
                }
                let row = tl
                    .items
                    .iter()
                    .rev()
                    .find(|r| r.message_id == inbox_id && r.role == TimelineRole::User)?;
                tl.sent_images.get(&row.id).map(Vec::as_slice)
            })
            .unwrap_or_default()
    }

    pub(super) fn pending_queue_echoes(&self, asid: &str) -> impl Iterator<Item = &Echo> {
        let timeline = self.timelines.get(asid);
        self.echoes_of(asid).iter().filter(move |echo| {
            echo.delivery.is_some()
                && echo.state != EchoState::Failed
                && !(echo.delivery == Some(BusyDelivery::Steer) && echo.state == EchoState::Sent)
                && !echo
                    .inbox_id
                    .as_ref()
                    .is_some_and(|id| timeline.is_some_and(|tl| tl.holds_inbox(id)))
        })
    }

    pub(super) fn forget_queue_echo(&mut self, asid: &str, id: &str) {
        if let Some(echoes) = self.echoes.get_mut(asid) {
            echoes.retain(|echo| echo.inbox_id.as_deref() != Some(id));
        }
    }

    /// `asid`'s timeline changed: drop the echoes its new rows stand for.
    pub fn echo_rows(&mut self, asid: &str) {
        let (Some(echoes), Some(tl)) = (self.echoes.get_mut(asid), self.timelines.get_mut(asid))
        else {
            return;
        };
        if reconcile(asid, echoes, &mut tl.items, &mut tl.sent_images) {
            tl.regroup();
            self.echo_rev += 1;
        }
    }

    pub fn echoes_of(&self, asid: &str) -> &[Echo] {
        self.echoes.get(asid).map_or(&[], Vec::as_slice)
    }

    pub fn timeline_echoes(&self, asid: &str) -> impl Iterator<Item = &Echo> {
        self.echoes_of(asid)
            .iter()
            .filter(|echo| echo.delivery.is_none() || echo.state == EchoState::Failed)
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
        echo.inbox_id = None;
        echo.token = next;
        echo.state = EchoState::Sending;
        self.echo_tokens.insert(next);
        echo.delivery = busy.then(|| echo.delivery.unwrap_or(BusyDelivery::Steer));
        let delivery = echo.delivery.map(BusyDelivery::wire);
        echo.at = Instant::now();
        let (text, files) = (echo.text.clone(), echo.files.clone());
        self.echo_rev += 1;
        self.send(Command::Compose(ComposeCommand::Submit {
            token: next,
            target: Target::Session(AgentSessionId(asid.to_string())),
            deliver: Deliver::Prompt(text),
            attachments: files,
            delivery,
            auto_approve: false,
        }));
    }

    fn discard_echo(&mut self, asid: &str, token: u64) {
        if let Some(v) = self.echoes.get_mut(asid) {
            v.retain(|e| e.token != token);
            self.echo_rev += 1;
        }
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
                    && !busy.contains(asid)
                    && e.at.elapsed() >= IDLE_TTL;
                if !stale && e.state == EchoState::Sent && !busy.contains(asid) {
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
    let paths: Vec<PathBuf> = echo
        .images
        .iter()
        .map(|path| {
            crate::model::media::cached_upload(cx.s.current_computer(), path)
                .unwrap_or_else(|| PathBuf::from(path))
        })
        .collect();
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
                .child(
                    crate::ui::art::ActivityMark::Compact
                        .build(k)
                        .key("echo-spin"),
                )
                .child(note(cx.t("Sending…"), k.c.text_faint)),
        ),
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
        Echo::new(token, text.into(), vec![], vec![], None, rows)
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
        assert!(!reconcile("s", &mut echoes, &mut rows, &mut HashMap::new()));
        // The first send's row arrives, its file list appended by the gateway.
        rows.push(user(
            "u1",
            "run the tests\n\nAttached files (on this host):\n- /up/a.png",
        ));
        assert!(reconcile("s", &mut echoes, &mut rows, &mut HashMap::new()));
        assert_eq!(echoes.iter().map(|e| e.token).collect::<Vec<_>>(), [2]);
        // The same rows again (a later snapshot) leave the second echo be.
        assert!(!reconcile("s", &mut echoes, &mut rows, &mut HashMap::new()));
        rows.push(user("u2", "run the tests"));
        assert!(reconcile("s", &mut echoes, &mut rows, &mut HashMap::new()));
        assert!(echoes.is_empty());
    }

    #[test]
    fn canonical_upload_url_replaces_only_its_own_image_echo() {
        let sent = |token, path: &str| {
            Echo::new(
                token,
                "[Image #1] review".into(),
                vec![path.into()],
                vec![path.into()],
                None,
                &[],
            )
        };
        let mut echoes = vec![sent(1, "/uploads/a.jpg"), sent(2, "/uploads/b.jpg")];
        let mut row = user("delivered", "[Image #1] review");
        let id = muqun_gateway::agents::message_images::original_asset_id("s", "/uploads/b.jpg");
        row.attachments = Some(vec![format!("/api/agent-images/{id}/original")]);
        let mut rows = [row];
        let mut images = HashMap::new();
        assert!(!reconcile(
            "other-session",
            &mut echoes,
            &mut rows,
            &mut images
        ));
        assert!(reconcile("s", &mut echoes, &mut rows, &mut images));
        assert_eq!(echoes.iter().map(|e| e.token).collect::<Vec<_>>(), [1]);
        assert!(!reconcile("s", &mut echoes, &mut rows, &mut images));
    }

    #[test]
    fn held_prompt_has_only_an_editable_receipt_until_delivery() {
        use crate::model::compose::InboxItem;
        use muqun_gateway::agents::AgentDomainEvent;
        for mode in [BusyDelivery::Queue, BusyDelivery::Steer] {
            let (tx, _rx) = crate::model::commands::channel();
            let mut s = UiState::new(tx);
            s.selected = Some("s1".into());
            let asid = AgentSessionId("s1".into());
            let text = "install on my phone";
            s.timelines
                .entry(asid.0.clone())
                .or_default()
                .replace(vec![user("previous", text)]);
            s.echo_sent(&asid.0, 42, text.into(), &[], Some(mode));
            assert_eq!(s.timeline_echoes(&asid.0).count(), 0);
            let inbox_item =
                serde_json::json!({"id":"m-next", "payload":{"text":text}, "delivery":mode.wire()});
            let inbox = |items, seq| AgentDomainEvent::InboxChanged {
                asid: asid.clone(),
                items,
                seq,
            };
            s.apply_event(inbox(vec![inbox_item.clone()], 8));
            s.echo_submitted(42, true);
            s.apply_event(AgentDomainEvent::TimelineUpsert {
                asid: asid.clone(),
                items: vec![user("next", text)],
                seq: 9,
            });
            assert_eq!(s.timelines["s1"].blocks.len(), 1);
            assert_eq!(s.queue, InboxItem::list(std::slice::from_ref(&inbox_item)));
            s.apply_event(inbox(vec![], 10));
            assert_eq!(s.timelines["s1"].blocks.len(), 2);
            s.apply_event(inbox(vec![inbox_item], 8));
            assert!(s.queue.is_empty());
            assert_eq!(s.timelines["s1"].blocks.len(), 2);
        }
    }

    #[test]
    fn image_receipts_transfer_to_the_queue_or_delivered_row_once_per_send() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.selected = Some("images".into());
        let asid = AgentSessionId("images".into());
        let photo = crate::model::compose::StagedFile {
            path: "/uploads/photo.png".into(),
            name: "photo.png".into(),
            image: true,
            size: 16,
            draft_token: None,
        };
        for token in [1, 2] {
            s.echo_sent(
                &asid.0,
                token,
                "".into(),
                std::slice::from_ref(&photo),
                Some(BusyDelivery::Queue),
            );
            assert!(s.echo_submitted(token, true));
        }
        let pending = |s: &UiState| {
            s.pending_queue_echoes(&asid.0)
                .map(|e| (e.token, e.images.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            pending(&s),
            vec![(1, vec![photo.path.clone()]), (2, vec![photo.path.clone()])]
        );
        let queued = crate::model::compose::InboxItem::from_value(&serde_json::json!({
            "id":"m-first", "delivery":"queue", "payload":{"text":""}
        }))
        .unwrap();
        s.apply_session_inbox(&asid.0, vec![queued.clone()], 3);
        s.apply_session_inbox(&asid.0, vec![queued.clone()], 4);
        assert_eq!(pending(&s), vec![(2, vec![photo.path.clone()])]);
        s.apply_session_inbox(&asid.0, vec![], 5);
        assert_eq!(pending(&s).len(), 2);
        let tl = s.timelines.get_mut(&asid.0).unwrap();
        tl.upsert(vec![user("first", ""), user("second", "")]);
        s.echo_rows(&asid.0);
        assert!(pending(&s).is_empty());
        assert!(s.timelines[&asid.0]
            .items
            .iter()
            .all(|r| r.attachments.as_deref() == Some(std::slice::from_ref(&photo.path))));
        let tl = s.timelines.get_mut(&asid.0).unwrap();
        tl.upsert(vec![user("second", "")]);
        tl.replace(vec![user("first", ""), user("second", "")]);
        assert!(tl
            .items
            .iter()
            .all(|r| r.attachments.as_deref() == Some(std::slice::from_ref(&photo.path))));
        s.apply_session_inbox(&asid.0, vec![], 5);
        assert_eq!(s.timelines[&asid.0].blocks.len(), 2);
    }

    #[test]
    fn accepted_image_steer_leaves_the_composer_before_the_transcript_arrives() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut state = UiState::new(tx);
        let image = crate::model::compose::StagedFile {
            path: "/uploads/chart.png".into(),
            name: "chart.png".into(),
            image: true,
            size: 16,
            draft_token: None,
        };
        state.echo_sent(
            "live",
            1,
            "[Image #1]".into(),
            &[image],
            Some(BusyDelivery::Steer),
        );
        assert_eq!(state.pending_queue_echoes("live").count(), 1);
        assert!(state.echo_submitted(1, true));
        assert_eq!(state.pending_queue_echoes("live").count(), 0);
    }

    #[test]
    fn direct_steer_gains_one_native_row_even_when_ack_arrives_last() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let id = "live";
        s.echo_sent(
            id,
            1,
            "change direction".into(),
            &[],
            Some(BusyDelivery::Steer),
        );
        let tl = s.timelines.entry(id.into()).or_default();
        tl.upsert(vec![user("instruction", "change direction")]);
        s.echo_rows(id);
        assert_eq!(s.timeline_echoes(id).count(), 0);
        assert!(s.echo_submitted(1, true));
        assert_eq!(s.timelines[id].blocks.len(), 1);
        assert!(s.echoes_of(id).is_empty());
    }

    #[test]
    fn a_send_echoes_at_once_and_a_late_ack_never_clears_the_next_draft() {
        use crate::model::compose::ComposerUpdate;
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.timelines.entry("s1".into()).or_default().replace(vec![]);
        let token = s.token(super::super::Slot::Session);
        s.echo_sent("s1", token, "ship it".into(), &[], None);
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
        s.echo_sent("s1", token, "and lint".into(), &[], None);
        s.on_composer(ComposerUpdate::Submitted { token, ok: false });
        assert_eq!(s.echoes_of("s1")[0].state, EchoState::Failed);
        assert_eq!(s.draft.text, "next thought");
    }

    #[test]
    fn stale_non_busy_echo_is_adopted_into_inbox_and_removed_from_timeline_echoes() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let id = "s-stale";
        s.timelines.entry(id.into()).or_default().replace(vec![]);
        // Sent while local `busy` flag lagged behind (`delivery: None`).
        s.echo_sent(id, 1, "wait your turn".into(), &[], None);
        assert_eq!(s.timeline_echoes(id).count(), 1);
        // Gateway announces the item is actually queued in the Steer inbox.
        s.echo_inbox(
            id,
            &[crate::model::compose::InboxItem {
                id: "inbox_99".into(),
                text: "wait your turn".into(),
                steer: true,
                files: vec![],
            }],
        );
        assert_eq!(
            s.timeline_echoes(id).count(),
            0,
            "once held in inbox, echo must not stay in timeline_echoes"
        );
        assert_eq!(s.echoes_of(id)[0].delivery, Some(BusyDelivery::Steer));
        assert_eq!(s.echoes_of(id)[0].inbox_id.as_deref(), Some("inbox_99"));
    }
}
