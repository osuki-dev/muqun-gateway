//! The Inbox: every permission, question and paused task waiting on the
//! owner, as a mail-style master–detail page. The list on the left groups
//! them by session, oldest wait first; the pane on the right shows the
//! selected one with its context and its answers. Answering moves on to the
//! one that slid into its place.

use std::collections::HashMap;

use muqun_gateway::agents::{
    AgentPart, FormRequest, PermissionDecision, PermissionRequest, TimelineRole,
};
use rune::{col, el, icon, row, text, El, Tx};
use rune_kit::shell::{RowStatus, SidebarRow};
use rune_kit::{Button, ButtonVariant, ControlSize, Keybind, Kit, Markdown, ScrollBar};

use super::copy::{tr, tr_ref};
use super::keymap::Action;
use super::parts::{approval, glyphs, question};
use super::{title_of, UiState};
use crate::config::Language;
use crate::hub::TaskRow;
use crate::model::commands::Command;
use crate::model::tool_view as tv;

/// The page's selection, and when each prompt first arrived.
#[derive(Debug, Default)]
pub struct InboxView {
    /// The selected item, as the owner last chose it (or the list moved it).
    pub pick: Option<String>,
    /// The list's order as last seen: when the picked item goes (answered
    /// here or elsewhere) the one after it is next.
    pub order: Vec<String>,
    /// When this window first saw each prompt, by item key.
    pub since: HashMap<String, u64>,
}

/// What an item asks of the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Plan,
    Question,
    Task,
}

impl Kind {
    fn word(self, language: Language) -> &'static str {
        match self {
            Kind::Permission => tr_ref(language, "Permission", "权限"),
            Kind::Plan => tr_ref(language, "Plan", "计划"),
            Kind::Question => tr_ref(language, "Question", "提问"),
            Kind::Task => tr_ref(language, "Paused task", "已暂停的任务"),
        }
    }

    fn icon(self) -> &'static rune::Icon {
        match self {
            Kind::Permission => &glyphs::TRUST_WARNING,
            Kind::Plan => &glyphs::TASK_LIST,
            Kind::Question => &glyphs::CHOICE,
            Kind::Task => &glyphs::WORKFLOW,
        }
    }
}

pub enum What<'a> {
    Permission(&'a PermissionRequest),
    Form(&'a FormRequest),
    Task(&'a TaskRow),
}

/// One thing waiting on the owner.
pub struct Item<'a> {
    pub key: String,
    /// The session it belongs to, when it is one's.
    pub asid: Option<&'a str>,
    pub kind: Kind,
    pub what: What<'a>,
    /// When it started waiting (ms), 0 when unknown.
    pub since: u64,
}

impl Item<'_> {
    /// The list groups by this: the session, or the task on its own.
    fn group(&self) -> String {
        self.asid.map_or_else(|| self.key.clone(), str::to_string)
    }
}

fn perm_kind(request: &PermissionRequest) -> Kind {
    let plan = |s: &str| s.to_ascii_lowercase().contains("plan");
    if plan(&request.action) || request.tool.as_deref().is_some_and(plan) {
        Kind::Plan
    } else {
        Kind::Permission
    }
}

/// Every item, grouped by session; the groups and the items in them oldest
/// wait first.
pub fn items(s: &UiState) -> Vec<Item<'_>> {
    let since = |key: &str| s.inbox_view.since.get(key).copied().unwrap_or(0);
    let mut out: Vec<Item<'_>> = Vec::new();
    for (asid, list) in &s.waiting_perms {
        for p in list {
            let key = format!("p:{}", p.id);
            out.push(Item {
                since: since(&key),
                key,
                asid: Some(asid.as_str()),
                kind: perm_kind(p),
                what: What::Permission(p),
            });
        }
    }
    for (asid, list) in &s.waiting_forms {
        for f in list {
            let key = format!("f:{}", f.id);
            out.push(Item {
                since: since(&key),
                key,
                asid: Some(asid.as_str()),
                kind: Kind::Question,
                what: What::Form(f),
            });
        }
    }
    // A waiting task whose session already shows its prompt is that prompt.
    for t in s.tasks.iter().filter(|t| t.state == "waiting") {
        if t.asid.as_deref().is_some_and(|a| s.session_waiting(a)) {
            continue;
        }
        out.push(Item {
            key: format!("t:{}", t.id),
            asid: None,
            kind: Kind::Task,
            what: What::Task(t),
            since: t.updated_ms,
        });
    }
    let mut first: HashMap<String, (u64, String)> = HashMap::new();
    for item in &out {
        let at = first
            .entry(item.group())
            .or_insert((u64::MAX, item.key.clone()));
        if item.since < at.0 || (item.since == at.0 && item.key < at.1) {
            *at = (item.since, item.key.clone());
        }
    }
    out.sort_by(|a, b| {
        let (ga, gb) = (&first[&a.group()], &first[&b.group()]);
        ga.cmp(gb)
            .then(a.since.cmp(&b.since))
            .then(a.key.cmp(&b.key))
    });
    out
}

/// The selected item's index: the pick, else what took its place.
pub fn current(items: &[Item<'_>], view: &InboxView) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let at = |key: &String| items.iter().position(|i| &i.key == key);
    let Some(pick) = &view.pick else {
        return Some(0);
    };
    if let Some(i) = at(pick) {
        return Some(i);
    }
    let Some(was) = view.order.iter().position(|k| k == pick) else {
        return Some(0);
    };
    view.order[was + 1..]
        .iter()
        .find_map(at)
        .or_else(|| view.order[..was].iter().rev().find_map(at))
        .or(Some(0))
}

/// After each batch of updates: stamp new prompts, forget answered ones,
/// and carry the selection over to what took the place of a gone pick.
pub fn note_waiting(s: &mut UiState) {
    let now = super::now_ms();
    let live: Vec<String> = s
        .waiting_perms
        .values()
        .flatten()
        .map(|p| format!("p:{}", p.id))
        .chain(
            s.waiting_forms
                .values()
                .flatten()
                .map(|f| format!("f:{}", f.id)),
        )
        .collect();
    s.inbox_view.since.retain(|k, _| live.contains(k));
    for key in live {
        s.inbox_view.since.entry(key).or_insert(now);
    }
    let list = items(s);
    let pick = current(&list, &s.inbox_view).map(|i| list[i].key.clone());
    let order = list.iter().map(|i| i.key.clone()).collect();
    s.inbox_view.pick = pick;
    s.inbox_view.order = order;
}

fn select(s: &mut UiState, key: String) {
    let order = items(s).iter().map(|i| i.key.clone()).collect();
    s.inbox_view.pick = Some(key);
    s.inbox_view.order = order;
}

/// The Inbox's keys (`keymap::run`): j/k and the arrows move, Enter opens
/// the session, A / Shift+A / D answer a permission.
pub fn key(s: &mut UiState, action: Action) -> bool {
    let list = items(s);
    let Some(at) = current(&list, &s.inbox_view) else {
        return false;
    };
    match action {
        Action::NextRow | Action::PrevRow => {
            let to = if action == Action::NextRow {
                (at + 1).min(list.len() - 1)
            } else {
                at.saturating_sub(1)
            };
            let key = list[to].key.clone();
            select(s, key);
            true
        }
        Action::OpenRow => {
            let asid = match &list[at].what {
                What::Task(t) => t.asid.clone(),
                _ => list[at].asid.map(str::to_string),
            };
            match asid {
                Some(asid) => {
                    s.open_session(&asid);
                    true
                }
                None => false,
            }
        }
        Action::Allow | Action::AllowAlways | Action::Deny => {
            let decision = match action {
                Action::Allow => PermissionDecision::Allow,
                Action::AllowAlways => PermissionDecision::AllowAlways,
                _ => PermissionDecision::Deny,
            };
            let What::Permission(request) = list[at].what else {
                return false;
            };
            if !approval::options(request)
                .iter()
                .any(|o| o.decision == decision)
            {
                return false;
            }
            let (request, key) = (request.clone(), list[at].key.clone());
            select(s, key);
            approval::reply(&request, decision)(s);
            true
        }
        _ => false,
    }
}

// ---- the view ------------------------------------------------------------------------

/// The list column's width on a card `card_w` wide.
fn list_width(card_w: f32) -> f32 {
    (card_w * 0.34).clamp(260.0, 360.0)
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let list = items(s);
    let at = current(&list, &s.inbox_view);
    let card_w = rune::memo::layout_with("main", |r| r[2]).unwrap_or(1080.0);
    let panes = if list.is_empty() {
        col()
            .key("inbox")
            .child(page_head(k, language, 0).max_w(420.0))
            .child(empty(k, language).grow(1.0).min_h(0.0))
    } else {
        row()
            .key("inbox")
            .child(list_pane(s, k, &list, at, list_width(card_w)))
            .child(
                el().key("inbox-rule")
                    .w(1.0)
                    .h_full()
                    .fill(k.c.border_weak)
                    .a11y_hidden(),
            )
            .child(match at {
                Some(i) => detail(s, k, &list[i]),
                None => el(),
            })
    };
    super::card_stage("inbox", panes)
}

fn page_head(k: &Kit, language: Language, count: usize) -> El {
    let mut title = row().key("inbox-title-line").gap(8.0).center_y().child(
        text(tr_ref(language, "Inbox", "收件箱"))
            .key("page-title")
            .font(rune::TextStyle {
                size: 22.0,
                line_height: 28.0,
                ..k.ty.ui_medium
            })
            .color(k.c.text_base),
    );
    if count > 0 {
        title = title.child(
            text(count.to_string())
                .key("inbox-count")
                .font(k.ty.ui_medium)
                .color(k.c.text_faint),
        );
    }
    col()
        .key("inbox-head")
        .w_full()
        .px(20.0)
        .pt(26.0)
        .pb(12.0)
        .gap(4.0)
        .child(title)
        .child(
            text(tr_ref(
                language,
                "Approvals and questions waiting on you.",
                "等待你批准或回答的事项。",
            ))
            .key("page-subtitle")
            .font(k.ty.small)
            .color(k.c.text_muted)
            .wrap(),
        )
}

/// The one line a list row leads with.
fn headline(language: Language, item: &Item<'_>) -> (String, String) {
    match &item.what {
        What::Permission(p) => {
            let subject = tv::permission_subject(p);
            let phrase = approval::action_phrase(language, &p.action);
            if item.kind == Kind::Plan {
                (tr(language, "Review the plan", "审阅计划"), subject)
            } else {
                (phrase, subject)
            }
        }
        What::Form(f) => {
            let n = f.fields.len();
            (
                f.title.clone(),
                tr(
                    language,
                    &format!("{n} field{}", if n == 1 { "" } else { "s" }),
                    &format!("{n} 项"),
                ),
            )
        }
        What::Task(t) => (
            t.title.clone(),
            t.progress.clone().unwrap_or_else(|| t.goal.clone()),
        ),
    }
}

fn list_pane(s: &UiState, k: &Kit, list: &[Item<'_>], at: Option<usize>, w: f32) -> El {
    let language = s.cfg.general.language;
    let now = super::now_ms();
    let mut rows = col()
        .key("inbox-list")
        .w_full()
        .px(10.0)
        .pb(16.0)
        .gap(1.0)
        .role("list")
        .label(tr_ref(language, "Waiting on you", "等你处理"));
    let mut group = String::new();
    for (i, item) in list.iter().enumerate() {
        let g = item.group();
        if g != group {
            rows = rows.child(group_head(s, k, item, i == 0));
            group = g;
        }
        let (title, sub) = headline(language, item);
        let subtitle = if sub.is_empty() {
            item.kind.word(language).to_string()
        } else {
            sub
        };
        let key = item.key.clone();
        rows = rows.child(
            SidebarRow::new(title)
                .avatar(
                    icon(item.kind.icon(), 14.0)
                        .color(if at == Some(i) {
                            k.c.icon_base
                        } else {
                            k.c.icon_muted
                        })
                        .a11y_hidden(),
                )
                .subtitle(subtitle)
                .detail(super::ago(item.since, now, language))
                .status(RowStatus::Attention)
                .selected(at == Some(i))
                .on_select(move |s: &mut UiState, _: &mut Tx| select(s, key.clone()))
                .build(k)
                .key(format!("inbox-row-{}", item.key)),
        );
    }
    col()
        .key("inbox-list-pane")
        .w(w)
        .h_full()
        .min_h(0.0)
        .child(page_head(k, language, list.len()))
        .child(
            col()
                .key("inbox-list-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .clip_fade(0.0, 12.0, 0.0, 0.0)
                .child(rows)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k),
        )
}

/// A group's quiet header: the session's agent mark and title.
fn group_head(s: &UiState, k: &Kit, item: &Item<'_>, first: bool) -> El {
    let language = s.cfg.general.language;
    let info = item.asid.and_then(|a| s.session_by_id(a));
    let (mark, title) = match (info, &item.what) {
        (Some(info), _) => (
            Some(super::agents::mark(&info.agent_id, 12.0, s.dark).a11y_hidden()),
            title_of(info),
        ),
        (None, What::Task(t)) => (
            t.agent_id
                .as_deref()
                .map(|a| super::agents::mark(a, 12.0, s.dark).a11y_hidden()),
            t.machine.clone().map_or_else(
                || tr(language, "Tasks", "任务"),
                |m| tr(language, &format!("Tasks on {m}"), &format!("{m} 上的任务")),
            ),
        ),
        (None, _) => (None, item.asid.unwrap_or_default().to_string()),
    };
    row()
        .key(format!("inbox-group-{}", item.group()))
        .w_full()
        .h(26.0)
        .mt(if first { 0.0 } else { 10.0 })
        .px(8.0)
        .gap(6.0)
        .center_y()
        .children(mark)
        .child(
            text(title.clone())
                .font(k.ty.small_medium)
                .color(k.c.text_faint)
                .truncate()
                .help_when_truncated(title)
                .shrink()
                .min_w(0.0),
        )
}

/// A button's label with its key after it, in the label's ink, quieter.
pub(super) fn keyed(
    label: String,
    keys: &'static [&'static str],
) -> impl FnOnce(rune::TextStyle, rune::Color) -> El {
    move |font, ink| {
        row()
            .gap(8.0)
            .center_y()
            .child(text(label).font(font).color(ink))
            .child(text(keys.join("")).font(font).color(ink.alpha(0.5)))
    }
}

fn button(
    k: &Kit,
    key: String,
    label: String,
    keys: &'static [&'static str],
    variant: ButtonVariant,
    press: impl Fn(&mut UiState) + 'static,
) -> El {
    Button::new(label.clone())
        .variant(variant)
        .label_with(keyed(label, keys))
        .shortcut(keys.join(" "))
        .on_press(press)
        .build(k)
        .key(key)
}

fn detail(s: &UiState, k: &Kit, item: &Item<'_>) -> El {
    let language = s.cfg.general.language;
    let look = s.look();
    let now = super::now_ms();
    let (title, _) = headline(language, item);
    let tone = match (&item.what, item.kind) {
        (What::Permission(p), _) if tv::dangerous_permission(p).is_some() => k.c.danger.icon,
        (_, Kind::Question) => k.c.info.icon,
        _ => k.c.warning.icon,
    };
    let session_asid = match &item.what {
        What::Task(t) => t.asid.clone(),
        _ => item.asid.map(str::to_string),
    };
    let info = session_asid.as_deref().and_then(|a| s.session_by_id(a));
    let agent = info
        .map(|i| i.agent_id.clone())
        .or_else(|| match &item.what {
            What::Task(t) => t.agent_id.clone(),
            _ => None,
        });

    // The eyebrow: what kind of thing, from whom, how long ago.
    let mut eyebrow = vec![item.kind.word(language).to_string()];
    if let Some(agent) = &agent {
        eyebrow.push(s.agent_name(agent));
    }
    let age = super::ago(item.since, now, language);
    if !age.is_empty() {
        eyebrow.push(age);
    }
    let mut body = col()
        .key(format!("inbox-detail-{}", item.key))
        .w_full()
        .max_w(760.0)
        .px(36.0)
        .pt(30.0)
        .pb(24.0)
        .gap(18.0)
        .child(
            col()
                .key("detail-head")
                .w_full()
                .gap(6.0)
                .child(
                    row()
                        .key("eyebrow")
                        .gap(6.0)
                        .center_y()
                        .child(icon(item.kind.icon(), 13.0).color(tone).a11y_hidden())
                        .child(
                            text(eyebrow.join(" · "))
                                .font(k.ty.small_medium)
                                .color(k.c.text_muted),
                        ),
                )
                .child(
                    text(title)
                        .key("detail-title")
                        .font(rune::TextStyle {
                            size: 20.0,
                            line_height: 26.0,
                            ..k.ty.ui_medium
                        })
                        .color(k.c.text_base)
                        .wrap()
                        .w_full(),
                ),
        );
    if let Some(info) = info {
        body = body.child(session_line(s, k, info));
    }
    body = body.child(rule(k, "detail-rule"));

    match &item.what {
        What::Permission(p) => {
            if let Some(reason) = tv::dangerous_permission(p) {
                body = body.child(
                    row()
                        .key("danger")
                        .w_full()
                        .gap(8.0)
                        .child(icon(&glyphs::WARNING, 14.0).color(k.c.danger.icon).mt(2.0))
                        .child(
                            text(approval::danger_reason(language, reason))
                                .font(k.ty.small_medium)
                                .color(k.c.danger.fg)
                                .wrap()
                                .shrink(),
                        ),
                );
            }
            let subject = tv::permission_subject(p);
            if let Some(what) = approval::what_block(k, p, &subject) {
                body = body.child(what);
            }
            if let Some(diff) = approval::permission_diff(k, p) {
                body = body.child(diff);
            }
            body = body.children(said(s, k, item.asid));
            body = body.child(answers(s, k, p));
        }
        What::Form(f) => {
            body = body.children(said(s, k, item.asid));
            body = body.child(question::form_fields(s, k, f, false));
        }
        What::Task(t) => {
            let note = t
                .progress
                .clone()
                .or_else(|| t.summary.clone())
                .unwrap_or_else(|| t.goal.clone());
            if !note.is_empty() {
                body = body.child(
                    text(note)
                        .font(k.ty.body)
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full(),
                );
            }
            let mut actions = row().key("answers").gap(8.0).flex_wrap();
            if t.completable {
                let id = t.id.clone();
                actions = actions.child(button(
                    k,
                    "task-done".into(),
                    tr(language, "Mark done", "标记完成"),
                    &["M"],
                    ButtonVariant::Submit,
                    move |s: &mut UiState| s.send(Command::CompleteTask { id: id.clone() }),
                ));
            }
            body = body.child(actions);
        }
    }
    let scroll = col()
        .key("inbox-detail-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(look.arrive(body, 0))
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_bar(k);
    col()
        .key("inbox-detail")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .child(scroll)
        .child(hints(s, k, item))
}

fn rule(k: &Kit, key: &str) -> El {
    el().key(key.to_string())
        .w_full()
        .h(1.0)
        .fill(k.c.border_weak)
        .a11y_hidden()
}

/// The session the item belongs to: its mark, title and folder, and a way in.
fn session_line(s: &UiState, k: &Kit, info: &muqun_gateway::agents::AgentSessionInfo) -> El {
    let language = s.cfg.general.language;
    let asid = info.asid.0.clone();
    let place = s
        .branch_of(info)
        .or_else(|| UiState::folder_name(info))
        .unwrap_or_default();
    let title = title_of(info);
    row()
        .key("session-line")
        .w_full()
        .gap(10.0)
        .center_y()
        .child(super::agents::mark(&info.agent_id, 16.0, s.dark).a11y_hidden())
        .child(
            text(title.clone())
                .font(k.ty.ui)
                .color(k.c.text_base)
                .truncate()
                .help_when_truncated(title)
                .shrink()
                .min_w(0.0),
        )
        .child(
            text(place)
                .font(k.ty.small)
                .color(k.c.text_faint)
                .truncate()
                .shrink()
                .min_w(0.0),
        )
        .child(el().grow(1.0))
        .child(
            Button::new(tr(language, "Open session", "打开会话"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .label_with(keyed(tr(language, "Open session", "打开会话"), &["↵"]))
                .on_press(move |s: &mut UiState| s.open_session(&asid))
                .build(k)
                .key("open-session"),
        )
}

/// What the agent last said in the session, when this window has its
/// timeline: the context the prompt came out of.
fn said(s: &UiState, k: &Kit, asid: Option<&str>) -> Option<El> {
    let language = s.cfg.general.language;
    let tl = s.timelines.get(asid?)?;
    let last = tl
        .items
        .iter()
        .rev()
        .find_map(|i| match (&i.role, &i.part) {
            (TimelineRole::Assistant, AgentPart::Text { text }) if !text.trim().is_empty() => {
                Some(text.as_str())
            }
            _ => None,
        })?;
    let (shown, _) = super::parts::cap_lines(last, 8);
    Some(
        col()
            .key("said")
            .w_full()
            .gap(6.0)
            .child(
                text(tr_ref(language, "The agent said", "Agent 说"))
                    .font(k.ty.small_medium)
                    .color(k.c.text_faint),
            )
            .child(
                row()
                    .key("said-body")
                    .w_full()
                    .gap(12.0)
                    .child(
                        el().key("said-bar")
                            .w(2.0)
                            .h_full()
                            .radius(1.0)
                            .fill(k.c.border_weak)
                            .a11y_hidden(),
                    )
                    .child(
                        col().key("said-text").grow(1.0).shrink().min_w(0.0).child(
                            Markdown::new(shown)
                                .selectable(true)
                                .on_link_tx(|s: &mut UiState, url: &str, tx: &mut rune::Tx| {
                                    crate::ui::parts::open_markdown_link(s, url, tx);
                                })
                                .build(k),
                        ),
                    ),
            ),
    )
}

/// A permission's answers, in the engine's order, each with its key.
fn answers(s: &UiState, k: &Kit, request: &PermissionRequest) -> El {
    let language = s.cfg.general.language;
    let danger = tv::dangerous_permission(request).is_some();
    let mut line = row().key("answers").w_full().gap(8.0).flex_wrap();
    for (i, option) in approval::options(request).iter().enumerate() {
        let (variant, keys): (ButtonVariant, &'static [&'static str]) = match option.decision {
            PermissionDecision::Allow if !danger => (ButtonVariant::Submit, &["A"]),
            PermissionDecision::Allow => (ButtonVariant::Outline, &["A"]),
            PermissionDecision::AllowAlways => (ButtonVariant::Outline, &["⇧", "A"]),
            PermissionDecision::Deny if danger => (ButtonVariant::Submit, &["D"]),
            PermissionDecision::Deny => (ButtonVariant::Ghost, &["D"]),
        };
        // Only the first answer of each kind owns the key.
        let first = approval::options(request)
            .iter()
            .position(|o| o.decision == option.decision)
            == Some(i);
        let label = approval::option_label(language, option);
        let press = approval::reply(request, option.decision);
        let el = if first {
            button(k, format!("answer-{i}"), label, keys, variant, press)
        } else {
            Button::new(label)
                .variant(variant)
                .on_press(press)
                .build(k)
                .key(format!("answer-{i}"))
        };
        line = line.child(el);
    }
    let mut block = col().key("answers-block").w_full().gap(8.0).child(line);
    if !request.save.is_empty() {
        block = block.child(
            text(format!(
                "{} {}",
                tr_ref(language, "Always allow covers", "始终允许将涵盖"),
                request.save.join(", ")
            ))
            .font(k.ty.small)
            .color(k.c.text_faint)
            .wrap(),
        );
    }
    block
}

/// The pane's foot: the keys that work here.
fn hints(s: &UiState, k: &Kit, item: &Item<'_>) -> El {
    let language = s.cfg.general.language;
    let hint = |keys: &[&str], what: &str| {
        row()
            .gap(6.0)
            .center_y()
            .child(Keybind::new(keys).build(k))
            .child(
                text(what.to_string())
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            )
    };
    let mut line = row()
        .key("inbox-hints")
        .w_full()
        .h(36.0)
        .px(20.0)
        .gap(16.0)
        .center_y()
        .child(hint(&["J", "K"], tr_ref(language, "Move", "移动")));
    if matches!(item.what, What::Permission(_)) {
        line = line
            .child(hint(&["A"], tr_ref(language, "Allow", "允许")))
            .child(hint(&["D"], tr_ref(language, "Deny", "拒绝")));
    }
    line = line.child(hint(&["↵"], tr_ref(language, "Open session", "打开会话")));
    let card_w = rune::memo::layout_with("main", |r| r[2]).unwrap_or(1080.0);
    if card_w >= 760.0 {
        line = line.child(hint(&["Esc"], tr_ref(language, "Back", "返回")));
    }
    col()
        .key("inbox-foot")
        .w_full()
        .child(rule(k, "foot-rule"))
        .child(line)
}

/// Nothing waiting: a calm line in the middle of the card.
fn empty(k: &Kit, language: Language) -> El {
    col()
        .key("inbox-empty")
        .w_full()
        .center()
        .gap(8.0)
        .child(
            row()
                .key("empty-mark")
                .size(40.0, 40.0)
                .radius(20.0)
                .center()
                .fill(k.alpha(k.c.success.icon, 0.12))
                .child(icon(&glyphs::DONE, 18.0).color(k.c.success.icon))
                .a11y_hidden(),
        )
        .child(
            text(tr_ref(language, "Nothing needs you", "没有需要你处理的事"))
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        )
        .child(
            text(tr_ref(
                language,
                "Approvals and questions from your agents land here.",
                "Agent 的审批请求与提问会出现在这里。",
            ))
            .font(k.ty.small)
            .color(k.c.text_muted),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands;

    fn perm(id: &str, asid: &str) -> PermissionRequest {
        serde_json::from_value(serde_json::json!({
            "id": id, "asid": asid, "action": "bash", "resources": ["true"],
            "prompt": "", "options": [],
        }))
        .expect("permission")
    }

    #[test]
    fn answering_moves_to_the_item_that_took_its_place() {
        let (tx, _rx) = commands::channel();
        let mut s = UiState::new(tx);
        s.waiting_perms.insert(
            "s1".into(),
            vec![perm("a", "s1"), perm("b", "s1"), perm("c", "s1")],
        );
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            s.inbox_view.since.insert(format!("p:{id}"), 10 + i as u64);
        }
        s.mode = super::super::Mode::Inbox;
        let selected = |s: &UiState| {
            let list = items(s);
            current(&list, &s.inbox_view).map(|i| list[i].key.clone())
        };
        assert_eq!(selected(&s).as_deref(), Some("p:a"));
        assert!(key(&mut s, Action::NextRow));
        assert_eq!(selected(&s).as_deref(), Some("p:b"));
        // Allowing b moves on to c, before and after the next update batch.
        assert!(key(&mut s, Action::Allow));
        assert_eq!(selected(&s).as_deref(), Some("p:c"));
        note_waiting(&mut s);
        assert_eq!(selected(&s).as_deref(), Some("p:c"));
        // The last one answered falls back to the one before it.
        assert!(key(&mut s, Action::Deny));
        assert_eq!(selected(&s).as_deref(), Some("p:a"));
        // No "always" was offered, so Shift+A does nothing.
        assert!(!key(&mut s, Action::AllowAlways));
    }
}
