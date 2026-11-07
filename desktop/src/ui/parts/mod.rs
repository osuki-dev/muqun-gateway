//! Timeline parts: one module per kind of [`AgentPart`] (and per tool family),
//! each drawing from the App's presentation rules (`agent-tool-card.tsx` and
//! friends). [`part`] picks the body; [`ToolCard`] is the shared shell every
//! tool call is drawn in.

pub mod approval;
pub mod diff_rows;
pub mod files;
pub mod glyphs;
pub mod images;
pub mod message;
pub mod muqun;
pub mod question;
pub mod tool;

use muqun_gateway::agents::{AgentPart, TimelineItem};
use rune::{col, icon, row, text, Color, Cursor, El, Icon};
use rune_kit::{Kit, Markdown, MarkdownStyle, Spinner};

use super::copy::tr_ref;
use super::timeline::SessionTimeline;
use super::UiState;
use crate::config::Language;

/// What every part builder needs: the state, the kit, and whose timeline.
pub struct Tcx<'a> {
    pub s: &'a UiState,
    pub k: &'a Kit,
    pub asid: &'a str,
    pub lang: Language,
}

impl<'a> Tcx<'a> {
    pub fn new(s: &'a UiState, k: &'a Kit, asid: &'a str) -> Self {
        Self {
            s,
            k,
            asid,
            lang: s.cfg.general.language,
        }
    }

    /// English or 简体中文, by the owner's language.
    pub fn t(&self, en: &'static str, zh: &'static str) -> &'static str {
        tr_ref(self.lang, en, zh)
    }

    pub fn zh(&self) -> bool {
        self.lang == Language::ZhCn
    }

    pub fn tl(&self) -> Option<&'a SessionTimeline> {
        self.s.timelines.get(self.asid)
    }

    pub fn is_open(&self, id: &str, default: bool) -> bool {
        self.tl().map_or(default, |t| t.is_open(id, default))
    }

    /// The session has stopped working: a call still pending was cut off.
    pub fn stopped(&self) -> bool {
        !self.s.session_busy(self.asid)
    }

    pub fn reduced_motion(&self) -> bool {
        self.s.cfg.appearance.reduced_motion
    }
}

/// A handler that flips card `id` of session `asid` open or closed.
pub fn flip(asid: &str, id: &str) -> impl Fn(&mut UiState) + 'static {
    let (asid, id) = (asid.to_string(), id.to_string());
    move |s: &mut UiState| {
        if let Some(tl) = s.timelines.get_mut(&asid) {
            tl.flip(&id);
        }
    }
}

/// The body of one timeline item.
pub fn part(cx: &Tcx, item: &TimelineItem) -> El {
    match &item.part {
        AgentPart::Tool(call) => tool::card(cx, item, call),
        AgentPart::Approval { request } => approval::permission_card(cx.s, cx.k, request),
        AgentPart::Form { request } => question::form_card(cx.s, cx.k, request),
        AgentPart::Diff { file, .. } => files::diff_part(cx, item, file),
        AgentPart::Todo { items } => {
            let items: Vec<(String, bool)> =
                items.iter().map(|t| (t.text.clone(), t.done)).collect();
            tool::todo_card(cx, &item.id, &items)
        }
        AgentPart::Shell { .. } => tool::shell_part(cx, item),
        _ => message::part(cx, item),
    }
}

// ---- shared pieces ----------------------------------------------------------

/// A small coloured label beside a card's title (`+3`, `exit 1`).
pub fn chip(k: &Kit, label: impl Into<String>, color: Color) -> El {
    text(label.into())
        .font(k.ty.small_medium)
        .color(color)
        .px(6.0)
        .py(1.0)
        .radius(k.radius.sm)
        .fill(k.alpha(color, 0.12))
}

/// Monospace text, capped to `max_lines`, with a note of what is held back.
pub fn mono(cx: &Tcx, key: &str, body: &str, max_lines: usize) -> El {
    let (shown, hidden) = cap_lines(body, max_lines);
    let mut block = col().key(key.to_string()).w_full().gap(4.0).child(
        text(shown)
            .font(cx.k.ty.mono)
            .color(cx.k.c.text_muted)
            .wrap_anywhere()
            .selectable(true)
            .selection_color(cx.k.selection())
            .w_full(),
    );
    if hidden > 0 {
        block = block.child(more_lines(cx, hidden));
    }
    block
}

/// Syntax-coloured code (a Markdown fence: Rune's highlighter), capped.
pub fn code(cx: &Tcx, key: &str, body: &str, language: Option<&str>, max_lines: usize) -> El {
    let (shown, hidden) = cap_lines(body, max_lines);
    let fence = if shown.contains("```") { "~~~~" } else { "```" };
    let src = format!("{fence}{}\n{shown}\n{fence}", language.unwrap_or(""));
    let mut block = col().key(key.to_string()).w_full().gap(4.0).child(
        // The card's body is the surface; the code draws no box of its own.
        Markdown::new(&src)
            .selectable(true)
            .style(MarkdownStyle::from_kit(cx.k).code_block(
                cx.k.alpha(cx.k.c.layer_01, 0.0),
                cx.k.alpha(cx.k.c.layer_01, 0.0),
            ))
            .build(cx.k),
    );
    if hidden > 0 {
        block = block.child(more_lines(cx, hidden));
    }
    block
}

/// Prose an agent wrote, as Markdown.
pub fn prose(cx: &Tcx, key: &str, body: &str) -> El {
    Markdown::new(body)
        .selectable(true)
        .build(cx.k)
        .key(format!("md-{key}"))
}

pub fn more_lines(cx: &Tcx, hidden: usize) -> El {
    text(if cx.zh() {
        format!("还有 {hidden} 行")
    } else if hidden == 1 {
        "1 more line".to_string()
    } else {
        format!("{hidden} more lines")
    })
    .font(cx.k.ty.small)
    .color(cx.k.c.text_faint)
}

/// "… N more" under a bounded list.
pub fn more_count(cx: &Tcx, hidden: usize) -> El {
    text(format!("… {hidden}"))
        .font(cx.k.ty.small)
        .color(cx.k.c.text_faint)
}

/// The first `max` lines and how many more there were.
pub fn cap_lines(body: &str, max: usize) -> (String, usize) {
    let total = body.lines().count();
    if total <= max {
        return (body.trim_end_matches('\n').to_string(), 0);
    }
    (
        body.lines().take(max).collect::<Vec<_>>().join("\n"),
        total - max,
    )
}

/// A caption-sized label.
pub fn caption(k: &Kit, s: impl Into<String>) -> El {
    text(s.into()).font(k.ty.small).color(k.c.text_faint)
}

/// A labelled value (`Exit code  3`).
pub fn fact(cx: &Tcx, label: &str, value: impl Into<String>) -> El {
    row()
        .gap(8.0)
        .w_full()
        .child(
            text(label.to_string())
                .font(cx.k.ty.small)
                .color(cx.k.c.text_faint),
        )
        .child(
            text(value.into())
                .font(cx.k.ty.small)
                .color(cx.k.c.text_base)
                .wrap_anywhere()
                .selectable(true)
                .shrink(),
        )
}

/// How a card's call is doing, for the mark at its header's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    Running,
    Done,
    Failed,
    Stopped,
}

/// The shell every tool call is drawn in: a one-line header (glyph, verb,
/// title, folder, chips, state, chevron) that opens and closes the body,
/// an always-visible preview (images, a question to answer), and the body.
pub struct ToolCard<'a> {
    pub id: &'a str,
    pub glyph: &'static Icon,
    pub verb: String,
    pub title: String,
    pub caption: String,
    pub chips: Vec<El>,
    pub state: CardState,
    /// Open when the reader has not said otherwise.
    pub open_default: bool,
    pub preview: Option<El>,
    pub body: Option<El>,
    pub actions: Vec<El>,
    pub error: Option<String>,
    /// Tint the glyph (danger for a failed command, accent for an approval).
    pub accent: Option<Color>,
    /// Waiting on the reader (a question to answer): the border says so.
    pub attention: bool,
}

impl<'a> ToolCard<'a> {
    pub fn new(id: &'a str, glyph: &'static Icon, verb: impl Into<String>) -> Self {
        Self {
            id,
            glyph,
            verb: verb.into(),
            title: String::new(),
            caption: String::new(),
            chips: Vec::new(),
            state: CardState::Done,
            open_default: false,
            preview: None,
            body: None,
            actions: Vec::new(),
            error: None,
            accent: None,
            attention: false,
        }
    }

    pub fn build(self, cx: &Tcx) -> El {
        let k = cx.k;
        let has_body = self.body.is_some() || self.error.is_some();
        let open = has_body && cx.is_open(self.id, self.open_default);
        let glyph_color = match self.state {
            CardState::Failed => k.c.danger.icon,
            _ => self.accent.unwrap_or(k.c.icon_muted),
        };
        let mut header = row()
            .key("head")
            .w_full()
            .min_h(34.0)
            .px(10.0)
            .py(6.0)
            .gap(8.0)
            .center_y()
            .child(icon(self.glyph, 14.0).color(glyph_color).a11y_hidden());
        if !self.verb.is_empty() {
            header = header.child(
                text(self.verb)
                    .key("verb")
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted)
                    .single_line(),
            );
        }
        if !self.title.is_empty() {
            header = header.child(
                text(self.title)
                    .key("title")
                    .font(k.ty.ui_compact)
                    .color(k.c.text_base)
                    .single_line()
                    .truncate()
                    .shrink(),
            );
        }
        let caption = if self.caption.is_empty() {
            text("")
        } else {
            text(self.caption).font(k.ty.small).color(k.c.text_faint)
        };
        header = header.child(
            caption
                .key("caption")
                .single_line()
                .truncate_head()
                .shrink()
                .grow(1.0),
        );
        for (i, c) in self.chips.into_iter().enumerate() {
            header = header.child(c.key(format!("chip-{i}")));
        }
        header = header.child(state_mark(cx, self.state));
        if has_body {
            let turn = if open {
                0.0
            } else {
                -std::f32::consts::FRAC_PI_2
            };
            header = k
                .interactive(
                    header.child(
                        icon(&glyphs::CHEVRON, 12.0)
                            .key("chevron")
                            .color(k.c.icon_faint)
                            .rotate(turn)
                            .a11y_hidden(),
                    ),
                )
                .cursor(Cursor::Pointer)
                .expanded(open)
                .on_press(flip(cx.asid, self.id));
        }
        let mut card = col()
            .key(format!("card-{}", self.id))
            .w_full()
            .radius(k.radius.md)
            .border(
                1.0,
                if self.attention {
                    k.alpha(k.c.icon_accent, 0.5)
                } else {
                    k.c.border_weak
                },
            )
            .fill(k.c.surface_raised_base)
            .clip_rounded(k.radius.md)
            .child(header);
        if let Some(preview) = self.preview {
            card = card.child(
                col()
                    .key("preview")
                    .w_full()
                    .px(10.0)
                    .pb(10.0)
                    .child(preview),
            );
        }
        if open {
            let mut body = col()
                .key("body")
                .w_full()
                .px(12.0)
                .pt(10.0)
                .pb(12.0)
                .gap(10.0)
                .border(0.0, k.c.border_weak)
                .fill(k.c.surface_base);
            if let Some(err) = self.error {
                body = body.child(
                    text(err)
                        .font(k.ty.small)
                        .color(k.c.danger.fg)
                        .wrap()
                        .selectable(true)
                        .w_full(),
                );
            }
            if let Some(b) = self.body {
                body = body.child(b);
            }
            let body = if cx.reduced_motion() {
                body
            } else {
                body.transition(rune::Transition::opacity())
            };
            card = card.child(body);
        }
        if !self.actions.is_empty() {
            let mut line = row()
                .key("actions")
                .w_full()
                .px(8.0)
                .pb(8.0)
                .gap(6.0)
                .wrap();
            for (i, a) in self.actions.into_iter().enumerate() {
                line = line.child(a.key(format!("act-{i}")));
            }
            card = card.child(line);
        }
        card
    }
}

fn state_mark(cx: &Tcx, state: CardState) -> El {
    let k = cx.k;
    match state {
        CardState::Running => Spinner::new().size(12.0).build(k).key("state"),
        CardState::Failed => icon(&glyphs::DISMISS, 13.0)
            .key("state")
            .color(k.c.danger.icon)
            .help(cx.t("Failed", "失败")),
        CardState::Stopped => text(cx.t("Stopped", "已停止"))
            .key("state")
            .font(k.ty.small)
            .color(k.c.text_faint)
            .single_line(),
        CardState::Done => col().key("state"),
    }
}

/// A plain block with the card look, for parts that are not tool calls.
pub fn plate(k: &Kit, key: String) -> El {
    col()
        .key(key)
        .w_full()
        .radius(k.radius.md)
        .border(1.0, k.c.border_weak)
        .fill(k.c.surface_raised_base)
}
