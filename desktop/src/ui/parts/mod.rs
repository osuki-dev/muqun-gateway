//! Timeline parts: one module per kind of [`AgentPart`] (and per tool family),
//! each drawing from the App's presentation rules (`agent-tool-card.tsx` and
//! friends). [`part`] picks the body; [`ToolCard`] is the shared shell every
//! tool call is drawn in.

pub mod approval;
pub mod context;
pub mod detail;
pub mod diff_rows;
pub mod files;
pub mod glyphs;
pub mod group;
pub mod images;
pub mod message;
pub mod muqun;
pub mod pane;
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
    /// Drawing into the detail pane (everything a call has), not the
    /// timeline's dense rows.
    pub pane: bool,
    /// The timeline is a narrow column: rows drop their secondary words.
    pub narrow: bool,
}

impl<'a> Tcx<'a> {
    pub fn new(s: &'a UiState, k: &'a Kit, asid: &'a str) -> Self {
        Self {
            s,
            k,
            asid,
            lang: s.cfg.general.language,
            pane: false,
            narrow: false,
        }
    }

    /// The same, for the detail pane.
    pub fn for_pane(s: &'a UiState, k: &'a Kit, asid: &'a str) -> Self {
        Self {
            pane: true,
            ..Self::new(s, k, asid)
        }
    }

    /// Row `id` is the reader's: selected with the keyboard or open in the pane.
    pub fn selected(&self, id: &str) -> bool {
        self.tl().and_then(|t| t.selected.as_deref()) == Some(id)
            || self
                .s
                .inspect
                .as_ref()
                .is_some_and(|i| i.asid == self.asid && i.id == id)
    }

    /// English or 简体中文, by the owner's language.
    pub fn t(&self, en: &'static str, zh: &'static str) -> &'static str {
        tr_ref(self.lang, en, zh)
    }

    pub fn zh(&self) -> bool {
        self.lang.is_zh()
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

/// A small coloured label beside a row's title (`+3`, `exit 1`): plain
/// text, no pill, so a row of them stays quiet.
pub fn chip(k: &Kit, label: impl Into<String>, color: Color) -> El {
    text(label.into())
        .font(k.ty.small_medium)
        .color(color)
        .single_line()
}

/// Monospace text, capped to `max_lines`, with a note of what is held back.
pub fn mono(cx: &Tcx, key: &str, body: &str, max_lines: usize) -> El {
    // The pane has the room: it shows the whole output, on a well.
    let max_lines = if cx.pane {
        max_lines.max(4000)
    } else {
        max_lines
    };
    let (shown, hidden) = cap_lines(body, max_lines);
    let mut block = col().key(key.to_string()).w_full().gap(4.0);
    if cx.pane {
        block = block
            .px(10.0)
            .py(8.0)
            .radius(cx.k.radius.md)
            .fill(cx.k.c.layer_01);
    }
    let mut block = block.child(
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
    let max_lines = if cx.pane {
        max_lines.max(4000)
    } else {
        max_lines
    };
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

/// Open a URL or file path clicked from a Markdown link.
/// - Web and mailto URLs (`http://`, `https://`, `mailto:`) open in the default external application.
/// - Local file URLs (`file://...`) or workspace file paths open in the built-in file viewer.
pub fn open_markdown_link(s: &mut UiState, url: &str, tx: &mut rune::Tx) {
    let url = url.trim();
    if url.is_empty() {
        return;
    }

    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
        let _ = tx.open_url(url);
        return;
    }

    let file_target = url.strip_prefix("file://").unwrap_or(url);
    // Strip line fragment if present (e.g. #L10 or #L10-L20)
    let path = file_target.split('#').next().unwrap_or(file_target);
    s.open_timeline_file(path);
}

/// Prose an agent wrote, as Markdown.
pub fn prose(cx: &Tcx, key: &str, body: &str) -> El {
    Markdown::new(body)
        .selectable(true)
        .on_link_tx(|s: &mut UiState, url: &str, tx: &mut rune::Tx| {
            open_markdown_link(s, url, tx);
        })
        .build(cx.k)
        .key(format!("md-{key}"))
}

pub fn more_lines(cx: &Tcx, hidden: usize) -> El {
    text(if cx.zh() {
        crate::ui::copy::localize(cx.lang, format!("还有 {hidden} 行"))
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
    /// What the row's Copy puts on the clipboard (the command, the path).
    pub copy: Option<String>,
    /// A file the row's Open shows in the viewer.
    pub open: Option<String>,
    /// A file the row's Diff button shows in the session diff reviewer.
    pub diff: Option<String>,
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
            copy: None,
            open: None,
            diff: None,
        }
    }

    /// The call as the timeline shows it: a dense row (anything waiting on
    /// the reader stays a full card), or everything it has in the pane.
    pub fn build(self, cx: &Tcx) -> El {
        if cx.pane {
            self.build_pane(cx)
        } else if self.attention {
            self.build_card(cx)
        } else {
            self.build_row(cx)
        }
    }

    /// The one-line title: a multi-line command shows its first line.
    fn one_line(title: &str) -> String {
        let mut lines = title.lines().map(str::trim).filter(|l| !l.is_empty());
        let mut first = lines.next().unwrap_or("").to_string();
        if lines.next().is_some() {
            first.push_str(" …");
        }
        first
    }

    /// One dense line: glyph, verb, target (monospace; its folder or what it
    /// is for, muted), meta on the right, and on hover Copy / Open and the
    /// call's own actions. A click opens the call in the detail pane.
    fn build_row(self, cx: &Tcx) -> El {
        let k = cx.k;
        let id = self.id.to_string();
        let has_body = self.body.is_some() || self.error.is_some();
        let open = has_body && cx.is_open(&id, self.open_default);
        let failed = self.state == CardState::Failed;
        let glyph_color = if failed {
            k.c.danger.icon
        } else {
            self.accent.unwrap_or(k.c.icon_faint)
        };
        let selected = cx.selected(&id);
        let mut line = row()
            .key("line")
            .w_full()
            .h(ROW_H)
            .px(6.0)
            .gap(8.0)
            .center_y()
            .radius(k.radius.sm)
            .child(icon(self.glyph, 13.0).color(glyph_color).a11y_hidden());
        if !self.verb.is_empty() {
            line = line.child(
                text(self.verb)
                    .key("verb")
                    .font(k.ty.small_medium)
                    .color(if failed {
                        k.c.danger.fg
                    } else {
                        k.c.text_muted
                    })
                    .single_line(),
            );
        }
        let title = Self::one_line(&self.title);
        if !title.is_empty() {
            line = line.child(
                text(title)
                    .key("title")
                    .font(k.ty.mono)
                    .color(k.c.text_base)
                    .single_line()
                    .truncate()
                    .shrink(),
            );
        }
        let caption_text = if cx.narrow {
            String::new()
        } else {
            self.caption.lines().next().unwrap_or("").to_string()
        };
        line = line.child(if caption_text.is_empty() {
            col().key("caption").grow(1.0)
        } else {
            text(caption_text)
                .key("caption")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .single_line()
                .truncate_head()
                .shrink()
                .grow(1.0)
        });
        // The tray: what a hover offers, over the meta's leading edge.
        let mut tray = row()
            .key("tray")
            .gap(2.0)
            .center_y()
            .group_opacity(0.0, 1.0);
        for (i, a) in self.actions.into_iter().enumerate() {
            tray = tray.child(a.key(format!("act-{i}")));
        }
        if let Some(path) = self.open.clone().filter(|p| !p.is_empty()) {
            tray = tray.child(tray_button(
                cx,
                "open",
                &glyphs::OPEN,
                cx.t("Open file", "打开文件"),
                move |s: &mut UiState, _tx: &mut rune::Tx| s.open_timeline_file(&path),
            ));
        }
        if let Some(path) = self.diff.clone().filter(|p| !p.is_empty()) {
            let asid = cx.asid.to_string();
            let p = path.clone();
            tray = tray.child(tray_button(
                cx,
                "diff",
                &glyphs::DIFF,
                cx.t("View in Diff", "查看差异"),
                move |s: &mut UiState, tx: &mut rune::Tx| {
                    s.send(crate::model::commands::Command::OpenDiff {
                        asid: muqun_gateway::agents::AgentSessionId(asid.clone()),
                        file: Some(p.clone()),
                    });
                    s.show_diff = true;
                    if let Some(diff) = &mut s.diff {
                        if let Some(idx) = diff.file_index(&p) {
                            s.diff_active_file = idx;
                            diff.scroll_to_file(idx, tx);
                        }
                    }
                },
            ));
        }
        if let Some(copy) = self.copy.clone().filter(|c| !c.is_empty()) {
            tray = tray.child(tray_button(
                cx,
                "copy",
                &rune_kit::glyphs::COPY,
                cx.t("Copy", "复制"),
                move |_: &mut UiState, tx: &mut rune::Tx| tx.copy(copy.clone()),
            ));
        }
        line = line.child(tray);
        for (i, c) in self.chips.into_iter().enumerate() {
            line = line.child(c.key(format!("chip-{i}")));
        }
        line = line.child(state_mark(cx, self.state));
        if has_body {
            let turn = if open {
                0.0
            } else {
                -std::f32::consts::FRAC_PI_2
            };
            line = line.child(
                icon(&glyphs::CHEVRON, 11.0)
                    .key("chevron")
                    .color(k.c.icon_faint)
                    .rotate(turn)
                    .a11y_hidden(),
            );
        }
        if selected {
            line = line.fill(k.c.surface_raised_hover);
        }
        let (asid, open_id) = (cx.asid.to_string(), id.clone());
        let line = k
            .interactive(line)
            .cursor(Cursor::Pointer)
            .hover_group()
            .role("button")
            .expanded(open)
            .on_press_tx(move |s: &mut UiState, tx: &mut rune::Tx| {
                s.press_row_inline(&asid, &open_id, has_body, tx)
            });
        let asid = cx.asid.to_string();
        let menu_id = id.clone();
        let line = line.on_secondary_press(move |s: &mut UiState, press, _tx: &mut rune::Tx| {
            s.row_menu = Some(crate::ui::RowMenu {
                asid: asid.clone(),
                id: menu_id.clone(),
                point: press,
            });
        });
        let mut block = col()
            .key(format!("call-{id}"))
            .w_full()
            .gap(4.0)
            .child(line);
        // A picture, a presented file or a checklist is the point: in view.
        if let Some(preview) = self.preview {
            block = block.child(
                col()
                    .key("preview")
                    .w_full()
                    .pl(27.0)
                    .pb(4.0)
                    .child(preview),
            );
        }
        if open {
            if let Some(err) = self.error {
                block = block.child(
                    col()
                        .key("error")
                        .w_full()
                        .pl(27.0)
                        .pr(6.0)
                        .pb(4.0)
                        .child(text(err).font(k.ty.small).color(k.c.danger.fg)),
                );
            }
            if let Some(body) = self.body {
                block = block.child(
                    col()
                        .key("body")
                        .w_full()
                        .pl(27.0)
                        .pr(6.0)
                        .pb(4.0)
                        .child(body),
                );
            }
        }
        block
    }

    /// Everything the call has, for the detail pane: the full title and
    /// caption, meta, error, pictures, the body and its actions.
    fn build_pane(self, cx: &Tcx) -> El {
        let k = cx.k;
        let failed = self.state == CardState::Failed;
        let mut head = row().key("head").w_full().gap(8.0).center_y().child(
            icon(self.glyph, 15.0)
                .color(if failed {
                    k.c.danger.icon
                } else {
                    self.accent.unwrap_or(k.c.icon_muted)
                })
                .a11y_hidden(),
        );
        if !self.verb.is_empty() {
            head = head.child(
                text(self.verb)
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .single_line(),
            );
        }
        head = head.child(col().grow(1.0));
        for (i, c) in self.chips.into_iter().enumerate() {
            head = head.child(c.key(format!("chip-{i}")));
        }
        head = head.child(state_mark(cx, self.state));
        let mut pane = col()
            .key(format!("detail-{}", self.id))
            .w_full()
            .gap(12.0)
            .child(head);
        if !self.title.trim().is_empty() {
            pane = pane.child(
                text(self.title.trim().to_string())
                    .key("title")
                    .font(k.ty.mono)
                    .color(k.c.text_base)
                    .wrap_anywhere()
                    .selectable(true)
                    .selection_color(k.selection())
                    .w_full(),
            );
        }
        if !self.caption.trim().is_empty() {
            pane = pane.child(
                text(self.caption.trim().to_string())
                    .key("caption")
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .wrap_anywhere()
                    .selectable(true)
                    .w_full(),
            );
        }
        if let Some(err) = self.error {
            pane = pane.child(
                text(err)
                    .key("error")
                    .font(k.ty.small)
                    .color(k.c.danger.fg)
                    .wrap()
                    .selectable(true)
                    .w_full(),
            );
        }
        if let Some(preview) = self.preview {
            pane = pane.child(col().key("preview").w_full().child(preview));
        }
        if let Some(body) = self.body {
            pane = pane.child(col().key("body").w_full().child(body));
        }
        let mut actions = self.actions;
        if let Some(path) = self.diff.clone().filter(|p| !p.is_empty()) {
            let asid = cx.asid.to_string();
            let p = path.clone();
            actions.push(
                rune_kit::Button::new(cx.t("View in Diff", "查看差异"))
                    .variant(rune_kit::ButtonVariant::Ghost)
                    .control_size(rune_kit::ControlSize::Small)
                    .on_press_tx(move |s: &mut UiState, tx: &mut rune::Tx| {
                        s.send(crate::model::commands::Command::OpenDiff {
                            asid: muqun_gateway::agents::AgentSessionId(asid.clone()),
                            file: Some(p.clone()),
                        });
                        s.show_diff = true;
                        if let Some(diff) = &mut s.diff {
                            if let Some(idx) = diff.file_index(&p) {
                                s.diff_active_file = idx;
                                diff.scroll_to_file(idx, tx);
                            }
                        }
                    })
                    .build(k),
            );
        }
        if !actions.is_empty() {
            let mut line = row().key("actions").w_full().gap(6.0).wrap();
            for (i, a) in actions.into_iter().enumerate() {
                line = line.child(a.key(format!("act-{i}")));
            }
            pane = pane.child(line);
        }
        pane
    }

    /// The bordered card, for a call waiting on the reader.
    fn build_card(self, cx: &Tcx) -> El {
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
        // One line: a multi-line command or title shows its first line.
        let mut lines = self.title.lines().map(str::trim).filter(|l| !l.is_empty());
        let mut title = lines.next().unwrap_or("").to_string();
        if lines.next().is_some() {
            title.push_str(" …");
        }
        // A long title takes the row; its folder would only be an ellipsis.
        let caption_text = if title.chars().count() > 56 {
            String::new()
        } else {
            self.caption.lines().next().unwrap_or("").to_string()
        };
        if !title.is_empty() {
            header = header.child(
                text(title)
                    .key("title")
                    .font(k.ty.ui_compact)
                    .color(k.c.text_base)
                    .single_line()
                    .truncate()
                    .shrink(),
            );
        }
        // An empty caption is a spacer, never an ellipsis.
        header = header.child(if caption_text.is_empty() {
            col().key("caption").grow(1.0)
        } else {
            text(caption_text)
                .key("caption")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .single_line()
                .truncate_head()
                .shrink()
                .grow(1.0)
        });
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

/// A dense row's height.
pub const ROW_H: f32 = 24.0;

/// A small icon button for a row's hover tray.
pub fn tray_button(
    cx: &Tcx,
    key: &str,
    glyph: &'static Icon,
    label: &str,
    f: impl Fn(&mut UiState, &mut rune::Tx) + 'static,
) -> El {
    cx.k.interactive(
        row()
            .key(format!("tray-{key}"))
            .size(20.0, 20.0)
            .center()
            .radius(cx.k.radius.sm)
            .child(icon(glyph, 12.0).color(cx.k.c.icon_muted).a11y_hidden()),
    )
    .cursor(Cursor::Pointer)
    .role("button")
    .label(label.to_string())
    .help(label.to_string())
    .on_press_tx(f)
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

#[cfg(test)]
mod tests {
    #[test]
    fn file_urls_strip_scheme_and_fragments() {
        let raw = "file:///path/to/file.rs#L10-L25";
        let target = raw.strip_prefix("file://").unwrap_or(raw);
        let path = target.split('#').next().unwrap_or(target);
        assert_eq!(path, "/path/to/file.rs");
    }
}
