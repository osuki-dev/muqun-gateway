//! Muqun Desktop UI — assembled from rune-kit shell + controls.

use std::sync::Arc;

use muqun_gateway::agents::{AgentDomainEvent, AgentSessionId, AgentSessionInfo, TimelineItem};
use rune::{col, row, text, Cx, El, KeyEvent, ScrollSpec, TextEdit, Tx};
use rune_kit::shell::{
    is_sidebar_toggle, PaneState, Sidebar, SidebarRow, SidebarSection, SidebarToggle, SplitView,
    Toolbar,
};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, Markdown, Note, ScrollBar,
    TextArea,
};

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
    pub sidebar: PaneState,
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
            sidebar: PaneState::new(260.0).range(180.0, 420.0),
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
                    let mut earlier: Vec<_> =
                        items.into_iter().filter_map(bubble_from_item).collect();
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
        self.sessions
            .truncate(crate::model::types::SESSION_LIST_LIMIT);
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

pub fn on_key(s: &mut UiState, ev: &KeyEvent, _tx: &mut Tx) -> bool {
    if is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        true
    } else {
        false
    }
}

pub fn view(s: &UiState, cx: &mut Cx<UiState>) -> El {
    let k = Kit::neutral(s.dark || cx.system_dark());
    let sidebar = sessions_sidebar(s, &k);
    let main = col()
        .key("main")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(timeline(s, &k))
        .child(composer(s, &k));

    let hub = if s.hub_label.is_empty() {
        "hub…".to_string()
    } else {
        s.hub_label.clone()
    };

    SplitView::new(main)
        .sidebar(&s.sidebar, |s: &mut UiState| &mut s.sidebar, sidebar)
        .toolbar(
            Toolbar::new()
                .title(s.status.clone())
                .trailing(
                    Badge::new(hub)
                        .variant(BadgeVariant::Subtle)
                        .build(&k)
                        .key("hub"),
                ),
        )
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(8.0)
        .toolbar_in_panel()
        .sidebar_toggle(
            SidebarToggle::new()
                .size(ControlSize::Large)
                .in_header(),
        )
        .build(&k)
}

fn sessions_sidebar(s: &UiState, k: &Kit) -> El {
    let rows = s.sessions.iter().enumerate().map(|(i, session)| {
        let asid = session.asid.0.clone();
        let selected = s.selected.as_deref() == Some(asid.as_str());
        let title = if session.title.is_empty() {
            asid.clone()
        } else {
            session.title.clone()
        };
        SidebarRow::new(title)
            .subtitle(session.agent_id.clone())
            .selected(selected)
            .on_select(move |s: &mut UiState, _| {
                s.selected = Some(asid.clone());
                s.status = format!("Opening {asid}");
                commands::send(
                    &s.commands,
                    Command::OpenSession(AgentSessionId(asid.clone())),
                );
            })
            .build(k)
            .key(format!("session-{i}-{}", session.asid.0))
    });

    let mut sidebar = Sidebar::new().label("Sessions");
    if s.sessions.is_empty() {
        sidebar = sidebar.child(
            text("No sessions yet")
                .key("empty-sessions")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .px(12.0)
                .py(8.0),
        );
    } else {
        sidebar = sidebar.section(
            SidebarSection::new("Sessions")
                .count(s.sessions.len())
                .rows(rows),
        );
    }

    sidebar
        .footer(
            Button::new("Refresh")
                .control_size(ControlSize::Large)
                .variant(ButtonVariant::Outline)
                .on_press(|s: &mut UiState| {
                    s.status = "Refreshing…".into();
                    commands::send(&s.commands, Command::Refresh);
                })
                .build(k)
                .key("refresh")
                .w_full(),
        )
        .build(k)
}

fn timeline(s: &UiState, k: &Kit) -> El {
    let mut feed = col()
        .key("feed")
        .gap(12.0)
        .w_full()
        .px(24.0)
        .py(24.0);
    if s.bubbles.is_empty() {
        feed = feed.child(
            text("Select a session or wait for messages.")
                .key("empty-feed")
                .font(k.ty.ui)
                .color(k.c.text_muted),
        );
    }
    for bubble in &s.bubbles {
        let body = if looks_like_note(&bubble.body) {
            Note::new(&bubble.body).key(&bubble.id).build(k)
        } else {
            Markdown::new(&bubble.body).build(k).key(format!("md-{}", bubble.id))
        };
        feed = feed.child(
            col()
                .key(format!("bubble-{}", bubble.id))
                .w_full()
                .gap(6.0)
                .child(
                    Badge::new(bubble.role.clone())
                        .variant(BadgeVariant::Subtle)
                        .build(k),
                )
                .child(body),
        );
    }
    feed.scroll(ScrollSpec {
        stick_bottom: true,
    })
    .grow(1.0)
    .min_h(0.0)
    .scroll_bar(k)
}

fn composer(s: &UiState, k: &Kit) -> El {
    col()
        .key("composer")
        .w_full()
        .px(24.0)
        .py(12.0)
        .gap(8.0)
        .child(
            TextArea::new(&s.draft)
                .placeholder("Message…")
                .label("Draft")
                .width(None)
                .auto_grow(2, 8)
                .on_edit(|s: &mut UiState| &mut s.draft)
                .on_submit(|s: &mut UiState, tx: &mut Tx| {
                    submit_draft(s);
                    tx.blur();
                })
                .build(k)
                .key("draft")
                .w_full(),
        )
        .child(
            row()
                .key("composer-actions")
                .gap(8.0)
                .center_y()
                .child(
                    Button::new("Send")
                        .variant(ButtonVariant::Submit)
                        .control_size(ControlSize::Large)
                        .on_press(|s: &mut UiState| submit_draft(s))
                        .build(k)
                        .key("send"),
                )
                .child(
                    text("⌘/Ctrl+Enter to send")
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                ),
        )
}

fn looks_like_note(body: &str) -> bool {
    body.contains("```facts")
        || body.contains("```steps")
        || body.contains("```callout")
        || body.contains(":::facts")
        || body.contains(":::steps")
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
