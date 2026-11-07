//! Rune UI shell for Muqun Desktop (replaces Blinc).

use std::sync::Arc;

use muqun_gateway::agents::{AgentDomainEvent, AgentSessionId, AgentSessionInfo, TimelineItem};
use rune::tok::{self, radius, space, type_};
use rune::{col, el, row, text, text_input, Cx, El, ScrollSpec, TextEdit, Tx};
use rune_kit::markdown::Markdown;
use rune_kit::Kit;

use crate::model::commands::{self, Command, CommandSender};
use crate::model::inbox::{Inbox, Update};

/// What one chat bubble shows.
#[derive(Clone)]
pub struct Bubble {
    pub id: String,
    pub role: String,
    pub body: String,
}

/// Main-thread UI state owned by Rune.
pub struct UiState {
    pub sessions: Vec<AgentSessionInfo>,
    pub selected: Option<String>,
    pub bubbles: Vec<Bubble>,
    pub draft: TextEdit,
    pub status: String,
    pub hub_label: String,
    pub commands: CommandSender,
    pub dark: bool,
    kit: Kit,
}

impl UiState {
    pub fn new(commands: CommandSender) -> Self {
        let mut draft = TextEdit::new("");
        draft.focused = true;
        Self {
            sessions: Vec::new(),
            selected: None,
            bubbles: Vec::new(),
            draft,
            status: "Starting…".into(),
            hub_label: String::new(),
            commands,
            dark: true,
            kit: Kit::neutral(true),
        }
    }

    pub fn apply_updates(&mut self, updates: Vec<Update>) {
        for update in updates {
            match update {
                Update::Agents(agents) => {
                    let n = agents.len();
                    self.status = format!("{n} agent(s) discovered");
                }
                Update::AgentSessions { sessions, .. } => {
                    self.merge_sessions(sessions);
                }
                Update::SessionCreated(info) => {
                    self.merge_sessions(vec![*info]);
                }
                Update::SessionGone(asid) => {
                    self.sessions.retain(|s| s.asid.0 != asid);
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        self.selected = None;
                        self.bubbles.clear();
                    }
                }
                Update::Snapshot(snap) => {
                    self.selected = Some(snap.info.asid.0.clone());
                    self.merge_sessions(vec![snap.info.clone()]);
                    self.bubbles = snap
                        .timeline
                        .into_iter()
                        .filter_map(bubble_from_item)
                        .collect();
                    self.status = format!("{} messages", self.bubbles.len());
                }
                Update::Event(ev) => self.apply_event(*ev),
                Update::Error(err) => {
                    self.status = err.message;
                }
                Update::HubMode(mode) => {
                    self.hub_label = mode.to_string();
                }
                Update::Earlier { items, .. } => {
                    let mut earlier: Vec<_> = items.into_iter().filter_map(bubble_from_item).collect();
                    earlier.append(&mut self.bubbles);
                    self.bubbles = earlier;
                }
                _ => {}
            }
        }
    }

    fn merge_sessions(&mut self, incoming: Vec<AgentSessionInfo>) {
        for info in incoming {
            if let Some(slot) = self
                .sessions
                .iter_mut()
                .find(|s| s.asid.0 == info.asid.0)
            {
                *slot = info;
            } else {
                self.sessions.push(info);
            }
        }
        self.sessions
            .sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        self.sessions.truncate(crate::model::types::SESSION_LIST_LIMIT);
    }

    fn apply_event(&mut self, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::TimelineUpsert { asid, items, .. } => {
                if self.selected.as_deref() != Some(asid.0.as_str()) {
                    return;
                }
                for item in items {
                    if let Some(bubble) = bubble_from_item(item) {
                        if let Some(slot) = self.bubbles.iter_mut().find(|b| b.id == bubble.id) {
                            *slot = bubble;
                        } else {
                            self.bubbles.push(bubble);
                        }
                    }
                }
            }
            AgentDomainEvent::SessionUpdated { info, .. } => {
                self.merge_sessions(vec![*info]);
            }
            _ => {}
        }
    }
}

fn bubble_from_item(item: TimelineItem) -> Option<Bubble> {
    let body = match item.part {
        muqun_gateway::agents::AgentPart::Text { text } => text,
        muqun_gateway::agents::AgentPart::Reasoning { text, .. } => text,
        muqun_gateway::agents::AgentPart::Synthetic { text, .. } => text,
        muqun_gateway::agents::AgentPart::System { text, .. } => text,
        muqun_gateway::agents::AgentPart::Status { text } => text,
        muqun_gateway::agents::AgentPart::Skill { text, .. } => text,
        _ => return None,
    };
    if body.is_empty() {
        return None;
    }
    Some(Bubble {
        id: item.id,
        role: format!("{:?}", item.role),
        body,
    })
}

pub fn view(s: &UiState, cx: &mut Cx<UiState>) -> El {
    let t = tok::theme(s.dark || cx.system_dark());
    let sidebar = session_list(s, t);
    let main = col()
        .key("main")
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .fill(t.bg)
        .child(timeline(s, t))
        .child(composer(s, t));
    col()
        .key("root")
        .w_full()
        .h_full()
        .fill(t.bg)
        .child(
            row()
                .key("chrome")
                .w_full()
                .h(36.0)
                .px(space::S6)
                .center_y()
                .gap(space::S3)
                .fill(t.card)
                .border(1.0, t.card_border)
                .child(
                    text("Muqun")
                        .key("brand")
                        .font(type_::TITLE)
                        .color(t.text),
                )
                .child(
                    text(s.status.clone())
                        .key("status")
                        .font(type_::LABEL)
                        .color(t.text_secondary)
                        .grow(1.0),
                )
                .child(
                    text(if s.hub_label.is_empty() {
                        "hub…".into()
                    } else {
                        s.hub_label.clone()
                    })
                    .key("hub")
                    .font(type_::LABEL)
                    .color(t.text_secondary),
                ),
        )
        .child(
            row()
                .key("body")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(sidebar)
                .child(main),
        )
}

fn session_list(s: &UiState, t: tok::Theme) -> El {
    let mut list = col().key("sessions").gap(2.0).w_full();
    if s.sessions.is_empty() {
        list = list.child(
            text("No sessions yet")
                .key("empty-sessions")
                .font(type_::LABEL)
                .color(t.text_secondary)
                .px(space::S3)
                .py(space::S2),
        );
    }
    for (i, session) in s.sessions.iter().enumerate() {
        let asid = session.asid.0.clone();
        let selected = s.selected.as_deref() == Some(asid.as_str());
        let title = if session.title.is_empty() {
            asid.clone()
        } else {
            session.title.clone()
        };
        let agent = session.agent_id.clone();
        list = list.child(
            col()
                .key(format!("session-{i}"))
                .w_full()
                .px(space::S3)
                .py(space::S2)
                .gap(2.0)
                .radius(radius::MD)
                .fill(if selected { t.control_off } else { t.card })
                .hover(t.hover)
                .on_press({
                    let asid = asid.clone();
                    move |s: &mut UiState| {
                        s.selected = Some(asid.clone());
                        s.status = format!("Opening {asid}");
                        commands::send(
                            &s.commands,
                            Command::OpenSession(AgentSessionId(asid.clone())),
                        );
                    }
                })
                .child(text(title).font(type_::BODY).color(t.text))
                .child(text(agent).font(type_::LABEL).color(t.text_secondary)),
        );
    }
    col()
        .key("sidebar")
        .w(260.0)
        .h_full()
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(
            text("Sessions")
                .key("sidebar-title")
                .font(type_::LABEL)
                .color(t.text_secondary)
                .px(space::S3)
                .py(space::S3),
        )
        .child(
            list.scroll(ScrollSpec {
                stick_bottom: false,
            })
            .grow(1.0)
            .min_h(0.0),
        )
        .child(
            row()
                .key("refresh")
                .role("button")
                .label("Refresh")
                .h(32.0)
                .mx(space::S3)
                .mb(space::S3)
                .center()
                .radius(radius::MD)
                .fill(t.control_off)
                .hover(t.hover)
                .on_press(|s: &mut UiState| {
                    s.status = "Refreshing…".into();
                    commands::send(&s.commands, Command::Refresh);
                })
                .child(text("Refresh").font(type_::LABEL).color(t.text)),
        )
}

fn timeline(s: &UiState, t: tok::Theme) -> El {
    let mut feed = col().key("feed").gap(space::S3).w_full().px(space::S6).py(space::S6);
    if s.bubbles.is_empty() {
        feed = feed.child(
            text("Select a session or wait for messages.")
                .key("empty-feed")
                .font(type_::BODY)
                .color(t.text_secondary),
        );
    }
    for (i, bubble) in s.bubbles.iter().enumerate() {
        let md = Markdown::new(&bubble.body).build(&s.kit);
        feed = feed.child(
            col()
                .key(format!("bubble-{i}-{}", bubble.id))
                .w_full()
                .gap(space::S1)
                .px(space::S3)
                .py(space::S3)
                .radius(radius::MD)
                .fill(t.card)
                .border(1.0, t.card_border)
                .child(
                    text(bubble.role.clone())
                        .font(type_::LABEL)
                        .color(t.text_secondary),
                )
                .child(md.key(format!("md-{}", bubble.id)).w_full()),
        );
    }
    feed.scroll(ScrollSpec {
        stick_bottom: true,
    })
    .grow(1.0)
    .min_h(0.0)
}

fn composer(s: &UiState, t: tok::Theme) -> El {
    col()
        .key("composer")
        .w_full()
        .px(space::S6)
        .py(space::S3)
        .gap(space::S2)
        .border(1.0, t.card_border)
        .fill(t.card)
        .child(
            el()
                .key("draft-box")
                .w_full()
                .h(72.0)
                .px(space::S3)
                .py(space::S2)
                .radius(radius::MD)
                .fill(t.bg)
                .border(1.0, t.card_border)
                .child(
                    text_input(&s.draft, type_::BODY)
                        .key("draft")
                        .w_full()
                        .h_full()
                        .color(t.text)
                        .on_edit(|s: &mut UiState| &mut s.draft)
                        .on_submit(|s: &mut UiState, tx: &mut Tx| {
                            submit_draft(s);
                            tx.blur();
                        }),
                ),
        )
        .child(
            row()
                .key("composer-actions")
                .gap(space::S2)
                .child(
                    row()
                        .key("send")
                        .role("button")
                        .label("Send")
                        .h(32.0)
                        .px(space::S6)
                        .center()
                        .radius(radius::MD)
                        .fill(tok::accent(0, s.dark))
                        .on_press(|s: &mut UiState| submit_draft(s))
                        .child(text("Send").font(type_::LABEL).color(tok::theme(false).bg)),
                )
                .child(
                    text("Enter to send")
                        .font(type_::LABEL)
                        .color(t.text_secondary),
                ),
        )
}

fn submit_draft(s: &mut UiState) {
    let text = s.draft.text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some(asid) = s.selected.clone() else {
        s.status = "Pick a session first".into();
        return;
    };
    s.bubbles.push(Bubble {
        id: format!("local-{}", s.bubbles.len()),
        role: "User".into(),
        body: text.clone(),
    });
    s.draft = TextEdit::new("");
    s.draft.focused = true;
    s.status = "Sending…".into();
    commands::send(
        &s.commands,
        Command::SendPrompt {
            asid: AgentSessionId(asid),
            text,
        },
    );
}

/// Poll the inbox on a background thread and push batches into Rune state.
pub fn spawn_inbox_pump(inbox: Arc<Inbox>, handle: rune::AppHandle<UiState>) {
    inbox.set_wake(Arc::new({
        let handle = handle.clone();
        move || {
            handle.update(|_, _| {});
        }
    }));
    std::thread::Builder::new()
        .name("muqun-ui-inbox".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(16));
            let batch = inbox.take();
            if batch.is_empty() {
                continue;
            }
            handle.update(move |s, _| s.apply_updates(batch));
        })
        .expect("inbox pump");
}
