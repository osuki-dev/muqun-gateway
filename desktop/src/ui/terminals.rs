//! The Terminals page: the Herdr (and tmux) workspaces the gateway serves,
//! every pane with its agent and status, a live view of the chosen pane's
//! screen, and typing into it -- the App's terminal workspace on the desktop.
//!
//! The page never focuses, closes or rearranges the owner's panes; it reads
//! them, and types only what the owner sends from here
//! (`model/terminals.rs` has the reads and the sends).

use std::sync::Arc;

use rune::{col, el, rich, row, text, El, ScrollSpec, Span, TextEdit, Tx};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, TextInput};

use super::copy::{tr, tr_ref};
use super::{Mode, UiState};
use crate::config::Language;
use crate::model::commands::Command;
use crate::model::terminals::{
    Ink, Pane, PaneRef, Screen, Sgr, TerminalCommand, TerminalUpdate, TerminalsView, Workspace,
};

/// The App's terminal glyph (`assets/glyphs/terminal.svg`).
pub static TERMINAL: rune::Icon = rune::Icon {
    name: "terminal",
    view: 24.0,
    body: r#"<path d="m5 7 5 5-5 5m9 0h5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// What the page holds between frames.
#[derive(Default)]
pub struct TermState {
    pub view: Arc<TerminalsView>,
    /// The first list arrived.
    pub loaded: bool,
    pub selected: Option<PaneRef>,
    pub screen: Option<(PaneRef, Arc<Screen>)>,
    pub input: TextEdit,
    /// The last failure about the selected pane, until a read succeeds.
    pub error: Option<String>,
}

/// The keys the App's key row offers first, with what the gateway calls them.
const KEYS: [(&str, &str); 9] = [
    ("Esc", "esc"),
    ("Tab", "tab"),
    ("↑", "up"),
    ("↓", "down"),
    ("←", "left"),
    ("→", "right"),
    ("Ctrl+C", "ctrl+c"),
    ("⌫", "backspace"),
    ("↵", "enter"),
];

impl UiState {
    /// The page opened, or another pane was chosen: keep them fresh.
    pub fn watch_terminals(&mut self) {
        crate::model::terminals::set_visible(true);
        self.send(Command::Terminals(TerminalCommand::Watch {
            open: true,
            pane: self.terms.selected.clone(),
        }));
    }

    pub fn select_pane(&mut self, pane: PaneRef) {
        if self.terms.selected.as_ref() == Some(&pane) {
            return;
        }
        self.terms.selected = Some(pane);
        self.terms.error = None;
        self.terms.input = TextEdit::new("");
        self.terms.input.focused = true;
        self.watch_terminals();
    }

    pub fn on_terminals(&mut self, update: TerminalUpdate) {
        let t = &mut self.terms;
        match update {
            TerminalUpdate::List(view) => {
                t.loaded = true;
                // A pane that went away is no longer selected.
                if let Some(at) = t.selected.clone() {
                    if view.error.is_none() && view.pane(&at).is_none() {
                        t.selected = None;
                        t.screen = None;
                    }
                }
                t.view = view;
            }
            TerminalUpdate::Screen { pane, screen } => {
                if t.selected.as_ref() == Some(&pane) {
                    t.error = None;
                    t.screen = Some((pane, screen));
                }
            }
            TerminalUpdate::Failed { pane, message } => {
                if pane.is_none() || pane == t.selected {
                    t.error = Some(message);
                }
            }
        }
    }

    fn send_to_pane(&mut self, submit: bool) {
        let Some(pane) = self.terms.selected.clone() else {
            return;
        };
        let text = std::mem::take(&mut self.terms.input.text);
        self.terms.input = TextEdit::new("");
        self.terms.input.focused = true;
        if text.is_empty() && !submit {
            return;
        }
        self.send(Command::Terminals(TerminalCommand::SendText {
            pane,
            text,
            submit,
        }));
    }

    fn press(&mut self, key: &str) {
        if let Some(pane) = self.terms.selected.clone() {
            self.send(Command::Terminals(TerminalCommand::SendKeys {
                pane,
                keys: vec![key.to_string()],
            }));
        }
    }
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let t = &s.terms;
    let body = if let Some(err) = t.view.error.as_ref().filter(|_| t.loaded) {
        empty(
            k,
            tr(language, "Terminals need the gateway", "终端需要 Gateway"),
            tr(
                language,
                &format!(
                    "Start the Muqun gateway service on this computer to see its Herdr workspaces here. ({err})"
                ),
                &format!("在这台电脑上启动 Muqun Gateway 服务后，这里会显示它的 Herdr 工作区。（{err}）"),
            ),
        )
    } else if !t.loaded {
        empty(
            k,
            tr(language, "Reading terminals…", "正在读取终端…"),
            String::new(),
        )
    } else {
        row()
            .key("terms-split")
            .w_full()
            .h_full()
            .min_h(0.0)
            .child(list(s, k, language))
            .child(
                el().key("terms-rule")
                    .w(1.0)
                    .h_full()
                    .fill(k.c.border_muted),
            )
            .child(pane_view(s, k, language))
    };
    col()
        .key("terminals")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(body)
}

fn empty(k: &Kit, title: String, detail: String) -> El {
    let mut c = col()
        .key("terms-empty")
        .w_full()
        .grow(1.0)
        .center()
        .gap(6.0)
        .child(rune::icon(&TERMINAL, 28.0).color(k.c.icon_faint))
        .child(text(title).font(k.ty.ui_medium).color(k.c.text_base));
    if !detail.is_empty() {
        c = c.child(
            text(detail)
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap()
                .max_w(460.0)
                .align(rune::Align::Center),
        );
    }
    c
}

/// A status as a word and a colour: the App's agent states.
fn status(k: &Kit, language: Language, status: &str) -> Option<(String, rune::Color)> {
    Some(match status {
        "working" => (tr(language, "working", "工作中"), k.c.info.icon),
        "blocked" => (tr(language, "needs you", "需要你"), k.c.warning.icon),
        "done" => (tr(language, "done", "完成"), k.c.success.icon),
        "idle" => (tr(language, "idle", "空闲"), k.c.icon_faint),
        _ => return None,
    })
}

fn dot(color: rune::Color) -> El {
    el().size(7.0, 7.0).radius(3.5).fill(color)
}

fn list(s: &UiState, k: &Kit, language: Language) -> El {
    let t = &s.terms;
    let mut column = col()
        .key("terms-list-col")
        .w_full()
        .gap(14.0)
        .px(12.0)
        .py(14.0);
    let several = t.view.sessions.len() > 1;
    let mut any = false;
    for session in &t.view.sessions {
        if several {
            let mut head = row()
                .key(format!("sess-{}", session.id))
                .gap(6.0)
                .center_y()
                .px(6.0)
                .child(
                    text(session.label.to_uppercase())
                        .font(k.ty.small_medium)
                        .color(k.c.text_faint),
                );
            if !session.connected {
                head = head.child(
                    text(tr(language, "not running", "未运行"))
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                );
            }
            column = column.child(head);
        }
        if let Some(err) = &session.error {
            column = column.child(
                text(err.clone())
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .wrap()
                    .px(6.0),
            );
        }
        for ws in &session.workspaces {
            any = true;
            column = column.child(workspace(s, k, language, &session.id, ws));
        }
    }
    if !any {
        column = column.child(
            text(tr(language, "No terminals open", "没有打开的终端"))
                .font(k.ty.small)
                .color(k.c.text_faint)
                .px(6.0),
        );
    }
    col()
        .key("terms-list")
        .w(300.0)
        .h_full()
        .min_h(0.0)
        .child(column)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
}

fn workspace(s: &UiState, k: &Kit, language: Language, session: &str, ws: &Workspace) -> El {
    let mut head = row()
        .key("ws-head")
        .w_full()
        .gap(8.0)
        .center_y()
        .px(6.0)
        .pb(2.0);
    if let Some(n) = ws.number {
        head = head.child(
            text(n.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_faint)
                .min_w(12.0),
        );
    }
    head = head.child(
        text(ws.label.clone())
            .font(k.ty.ui_medium)
            .color(k.c.text_base)
            .truncate()
            .grow(1.0)
            .shrink(),
    );
    if let Some((_, color)) = status(k, language, &ws.status) {
        head = head.child(dot(color));
    }
    let mut block = col()
        .key(format!("ws-{session}-{}", ws.id))
        .w_full()
        .gap(2.0)
        .child(head);
    let tabbed = ws.tabs.len() > 1;
    for tab in &ws.tabs {
        if tabbed {
            block = block.child(
                text(tab.label.clone())
                    .key(format!("tab-{}", tab.id))
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .pl(26.0)
                    .pt(4.0),
            );
        }
        for pane in &tab.panes {
            block = block.child(pane_row(s, k, language, session, pane));
        }
    }
    block
}

fn pane_row(s: &UiState, k: &Kit, language: Language, session: &str, pane: &Pane) -> El {
    let at = PaneRef {
        session: session.to_string(),
        pane: pane.id.clone(),
    };
    let selected = s.terms.selected.as_ref() == Some(&at);
    let mark = match &pane.agent {
        Some(agent) => super::agents::mark(agent, 14.0, s.dark),
        None => rune::icon(&TERMINAL, 14.0).color(k.c.icon_muted),
    };
    let detail = pane
        .cwd
        .as_deref()
        .map(home_relative)
        .or_else(|| pane.title.clone())
        .unwrap_or_default();
    let mut words = col().grow(1.0).shrink().gap(1.0).child(
        text(display_name(s, pane))
            .font(k.ty.ui)
            .color(if selected {
                k.c.text_strong
            } else {
                k.c.text_base
            })
            .truncate(),
    );
    if !detail.is_empty() {
        words = words.child(
            text(detail)
                .font(k.ty.small)
                .color(k.c.text_faint)
                .truncate_head(),
        );
    }
    let mut line = row()
        .key(format!("pane-{}", pane.id))
        .w_full()
        .gap(8.0)
        .center_y()
        .pl(20.0)
        .pr(8.0)
        .py(5.0)
        .radius(k.radius.md)
        .child(mark)
        .child(words);
    if let Some((word, color)) = status(k, language, &pane.status) {
        line = line.child(
            row()
                .gap(5.0)
                .center_y()
                .child(dot(color))
                .child(text(word).font(k.ty.small).color(k.c.text_muted)),
        );
    }
    if selected {
        line = line.fill(k.c.surface_hover);
    }
    line.hover(k.c.hover)
        .role("button")
        .label(display_name(s, pane))
        .on_press(move |s: &mut UiState| s.select_pane(at.clone()))
}

fn display_name(s: &UiState, pane: &Pane) -> String {
    match &pane.agent {
        Some(agent) => s.agent_name(super::agents::brand_id(agent)),
        None => pane.name(),
    }
}

fn home_relative(path: &str) -> String {
    match dirs::home_dir().and_then(|h| {
        std::path::Path::new(path)
            .strip_prefix(h)
            .ok()
            .map(|r| r.display().to_string())
    }) {
        Some(rest) if rest.is_empty() => "~".into(),
        Some(rest) => format!("~/{rest}"),
        None => path.to_string(),
    }
}

fn ink(k: &Kit, ink: Ink) -> rune::Color {
    match ink {
        Ink::Faint => k.c.text_faint,
        Ink::Red => k.c.danger.icon,
        Ink::Green => k.c.success.icon,
        Ink::Yellow => k.c.warning.icon,
        Ink::Blue => k.c.info.icon,
        Ink::Magenta => k.c.text_accent,
        Ink::Cyan => k.c.info.icon,
        Ink::Bright => k.c.text_strong,
    }
}

fn spans(k: &Kit, runs: &[(std::ops::Range<usize>, Sgr)]) -> Vec<Span> {
    runs.iter()
        .map(|(r, sgr)| {
            let mut span = Span::new(r.start, r.end).mono();
            if let Some(c) = sgr.ink {
                span = span.color(ink(k, c));
            } else if sgr.dim {
                span = span.color(k.c.text_muted);
            }
            if sgr.bold {
                span = span.weight(650.0);
            }
            if sgr.italic {
                span = span.italic();
            }
            if sgr.underline {
                let line = span.color.unwrap_or(k.c.text_base);
                span = span.underline(line);
            }
            span
        })
        .collect()
}

fn pane_view(s: &UiState, k: &Kit, language: Language) -> El {
    let t = &s.terms;
    let Some(at) = t.selected.clone() else {
        return col().key("terms-pane").grow(1.0).h_full().child(empty(
            k,
            tr(language, "Choose a pane", "选择一个窗格"),
            tr(
                language,
                "Its screen shows here, live; what you type is sent to it.",
                "这里实时显示它的屏幕；你输入的内容会发送给它。",
            ),
        ));
    };
    let pane = t.view.pane(&at);
    let name = pane
        .map(|p| display_name(s, p))
        .unwrap_or_else(|| at.pane.clone());
    let place = t
        .view
        .place(&at)
        .map(|(w, tab)| {
            if w.tabs.len() > 1 {
                format!("{} · {}", w.label, tab.label)
            } else {
                w.label.clone()
            }
        })
        .unwrap_or_default();
    let mut sub = vec![place];
    if let Some(p) = pane {
        if let Some(cwd) = &p.cwd {
            sub.push(home_relative(cwd));
        }
        if p.agent.is_some() {
            if let Some(cmd) = &p.command {
                sub.push(cmd.clone());
            }
        }
    }
    let sub: Vec<String> = sub.into_iter().filter(|x| !x.is_empty()).collect();
    let mut head = row()
        .key("pane-head")
        .w_full()
        .gap(10.0)
        .center_y()
        .px(18.0)
        .py(12.0)
        .child(match pane.and_then(|p| p.agent.as_deref()) {
            Some(agent) => super::agents::mark(agent, 18.0, s.dark),
            None => rune::icon(&TERMINAL, 18.0).color(k.c.icon_muted),
        })
        .child(
            col()
                .grow(1.0)
                .shrink()
                .gap(1.0)
                .child(
                    text(name)
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .truncate(),
                )
                .child(
                    text(sub.join(" · "))
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .truncate(),
                ),
        );
    if let Some((word, _)) = pane.and_then(|p| status(k, language, &p.status)) {
        head = head.child(
            Badge::new(word)
                .variant(BadgeVariant::Subtle)
                .build(k)
                .key("pane-status"),
        );
    }

    let screen = match &t.screen {
        Some((p, screen)) if *p == at => rich(screen.text.clone(), spans(k, &screen.runs))
            .key("pane-screen-text")
            .font(k.ty.mono)
            .color(k.c.text_base)
            .selectable(true)
            .selection_color(k.selection()),
        _ => text(tr(language, "Reading the screen…", "正在读取屏幕…"))
            .key("pane-screen-text")
            .font(k.ty.small)
            .color(k.c.text_faint),
    };
    let screen = col()
        .key("pane-screen")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(k.c.bg_deep)
        .child(col().key("pane-screen-pad").padding(16.0).child(screen))
        .scroll_xy()
        .scroll(ScrollSpec { stick_bottom: true });

    let mut keys = row().key("pane-keys").gap(4.0).center_y().flex_wrap();
    for (label, key) in KEYS {
        keys = keys.child(
            Button::new(label)
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.press(key))
                .build(k)
                .key(format!("key-{key}")),
        );
    }
    let composer = row()
        .key("pane-composer")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            TextInput::new(&t.input)
                .placeholder(tr_ref(
                    language,
                    "Send text to this pane (Enter submits it)",
                    "发送文字到此窗格（回车即提交）",
                ))
                .label(tr_ref(language, "Send to pane", "发送到窗格"))
                .on_edit(|s: &mut UiState| &mut s.terms.input)
                .on_submit(|s: &mut UiState, _: &mut Tx| s.send_to_pane(true))
                .build(k)
                .key("pane-input")
                .grow(1.0),
        )
        .child(
            Button::new(tr_ref(language, "Send", "发送"))
                .variant(ButtonVariant::Submit)
                .on_press(|s: &mut UiState| s.send_to_pane(true))
                .build(k)
                .key("pane-send"),
        );
    let mut foot = col()
        .key("pane-foot")
        .w_full()
        .gap(8.0)
        .px(18.0)
        .py(12.0)
        .child(keys)
        .child(composer);
    if let Some(err) = &t.error {
        foot = foot.child(
            text(err.clone())
                .key("pane-error")
                .font(k.ty.small)
                .color(k.c.danger.icon)
                .wrap(),
        );
    }
    col()
        .key("terms-pane")
        .grow(1.0)
        .shrink()
        .h_full()
        .min_h(0.0)
        .child(head)
        .child(el().w_full().h(1.0).fill(k.c.border_muted))
        .child(screen)
        .child(el().w_full().h(1.0).fill(k.c.border_muted))
        .child(foot)
}

/// Leaving the page stops the reads (`Mode` changes from anywhere).
pub fn page_visible(mode: Mode) {
    crate::model::terminals::set_visible(mode == Mode::Terminals);
}
