//! The right-hand inspector: Queue, Git (with worktrees), Subagents, Saved, Shells.

use muqun_gateway::agents::AgentSessionId;
use rune::{col, el, row, text, El, ScrollSpec, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, ScrollBar, SegmentedControl,
};

use super::copy::{tr, tr_ref};
use super::{muted, timeline, title_of, InspectorTab, Mode, UiState};
use crate::config::Language;
use crate::model::commands::Command;
use crate::model::compose::ComposeCommand;

const TABS: [(&str, InspectorTab); 5] = [
    ("Queue", InspectorTab::Queue),
    ("Git", InspectorTab::Git),
    ("Agents", InspectorTab::Subagents),
    ("Saved", InspectorTab::Saved),
    ("Shells", InspectorTab::Shells),
];

fn small(
    k: &Kit,
    label: &str,
    variant: ButtonVariant,
    key: String,
    f: impl Fn(&mut UiState) + 'static,
) -> El {
    Button::new(label)
        .variant(variant)
        .control_size(ControlSize::Small)
        .on_press(f)
        .build(k)
        .key(key)
}

fn tab_label(language: Language, en: &'static str) -> &'static str {
    match en {
        "Queue" => tr_ref(language, "Queue", "队列"),
        "Git" => "Git",
        "Agents" => tr_ref(language, "Agents", "子 Agent"),
        "Saved" => tr_ref(language, "Saved", "已保存"),
        "Shells" => tr_ref(language, "Shells", "后台命令"),
        other => other,
    }
}

/// A quiet empty state: a word and a line under it, on its own sheet.
fn empty(k: &Kit, key: &str, title: &str, detail: &str) -> El {
    col()
        .key(format!("empty-{key}"))
        .w_full()
        .gap(2.0)
        .px(12.0)
        .py(12.0)
        .radius(crate::theme::look::CARD_RADIUS)
        .fill(k.c.layer_01.alpha(0.5))
        .border(1.0, k.c.border_muted)
        .border_dash(4.0, 4.0)
        .child(
            text(title.to_string())
                .font(k.ty.ui_medium)
                .color(k.c.text_muted),
        )
        .child(muted(k, detail))
}

/// One list row: a title, a muted second line, and an optional action.
fn item(k: &Kit, key: String, title: String, detail: String, action: Option<El>) -> El {
    let mut copy = col()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(2.0)
        .child(text(title).font(k.ty.ui).color(k.c.text_base).truncate());
    if !detail.is_empty() {
        copy = copy.child(
            text(detail)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .truncate(),
        );
    }
    let mut line = row()
        .key(key)
        .w_full()
        .gap(8.0)
        .center_y()
        .px(10.0)
        .py(6.0)
        .radius(crate::theme::look::CARD_RADIUS)
        .fill(k.c.layer_01.alpha(0.5))
        .border(1.0, k.c.border_muted)
        .child(copy);
    if let Some(action) = action {
        line = line.child(action);
    }
    line
}

/// A section heading with a hairline running on from it.
fn heading(k: &Kit, label: &str) -> El {
    row()
        .key(format!("h-{label}"))
        .w_full()
        .gap(8.0)
        .center_y()
        .mt(6.0)
        .child(
            text(label.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_faint),
        )
        .child(el().grow(1.0).h(1.0).fill(k.c.border_muted).a11y_hidden())
}

fn queue(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let t = |en: &'static str, zh: &'static str| tr_ref(language, en, zh);
    let mut out = Vec::new();
    let asid = s.selected.clone().unwrap_or_default();
    let perms = s
        .waiting_perms
        .get(&asid)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let forms = s
        .waiting_forms
        .get(&asid)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if !perms.is_empty() || !forms.is_empty() {
        out.push(heading(k, t("Waiting on you", "等你处理")));
        out.extend(
            perms
                .iter()
                .map(|p| timeline::permission_card(s, k, p).key(format!("wp-{}", p.id))),
        );
        out.extend(
            forms
                .iter()
                .map(|f| timeline::form_card(s, k, f).key(format!("wf-{}", f.id))),
        );
    }
    out.push(heading(k, t("Queued prompts", "排队的消息")));
    if s.queue.is_empty() {
        out.push(empty(
            k,
            "queue",
            t("Nothing queued", "没有排队的消息"),
            t(
                "Messages sent while the agent works line up here.",
                "Agent 工作时发送的消息会在这里排队。",
            ),
        ));
    }
    for q in &s.queue {
        let (id, a) = (q.id.clone(), AgentSessionId(asid.clone()));
        let steer = q.steer;
        let (id2, a2) = (id.clone(), a.clone());
        let mut action = row().gap(4.0);
        if q.files.is_empty() {
            let (edit_id, text) = (q.id.clone(), q.text.clone());
            action = action.child(small(
                k,
                t("Edit", "编辑"),
                ButtonVariant::Ghost,
                format!("ed-{edit_id}"),
                move |s| {
                    super::composer::edit_queued(s, &edit_id, text.clone());
                },
            ));
        }
        action = action
            .child(small(
                k,
                if steer {
                    t("Keep queued", "保持排队")
                } else {
                    t("Send next", "下一个发送")
                },
                ButtonVariant::Ghost,
                format!("dl-{id}"),
                move |s| {
                    if let Some(item) = s.queue.iter_mut().find(|x| x.id == id) {
                        item.steer = !steer;
                    }
                    s.send(Command::Compose(ComposeCommand::InboxDelivery {
                        asid: a.clone(),
                        item: id.clone(),
                        steer: !steer,
                    }));
                },
            ))
            .child(small(
                k,
                t("Cancel", "取消"),
                ButtonVariant::Ghost,
                format!("cx-{id2}"),
                move |s| {
                    s.queue.retain(|x| x.id != id2);
                    s.send(Command::Compose(ComposeCommand::InboxCancel {
                        asid: a2.clone(),
                        item: id2.clone(),
                    }));
                },
            ));
        let images = q.files.iter().filter(|f| f.image).count();
        let detail = format!(
            "{}{}",
            if q.steer {
                t("sends next", "下一个发送")
            } else {
                t("queued", "已排队")
            },
            if q.files.is_empty() {
                String::new()
            } else if images > 0 {
                format!(" · {} image(s)", images)
            } else {
                format!(" · {} file(s)", q.files.len())
            }
        );
        let mut card = item(
            k,
            format!("q-{}", q.id),
            timeline::clip(&q.text, 80),
            detail,
            Some(action),
        );
        let thumbs: Vec<&str> = q
            .files
            .iter()
            .filter(|f| f.image)
            .map(|f| f.path.as_str())
            .collect();
        if let Some(row) = super::composer::queue_thumbs(s, k, &q.id, &thumbs) {
            card = col()
                .key(format!("q-wrap-{}", q.id))
                .w_full()
                .gap(6.0)
                .child(row)
                .child(card);
        }
        out.push(card);
    }
    out
}

fn git(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let t = |en: &'static str, zh: &'static str| tr_ref(language, en, zh);
    let mut out = vec![heading(k, t("Branch", "分支"))];
    let info = s.current();
    let cwd = info.and_then(|i| i.directory.clone());
    match cwd.as_ref().and_then(|c| s.branches.get(c)) {
        Some(b) => {
            out.push(
                Badge::new(b.current.clone().unwrap_or_else(|| "detached".into()))
                    .variant(BadgeVariant::Accent)
                    .build(k),
            );
            if !b.local.is_empty() {
                out.push(muted(k, format!("{} local branch(es)", b.local.len())));
            }
        }
        None => out.push(empty(
            k,
            "branch",
            t("No repository", "没有仓库"),
            t(
                "This folder is not a git checkout.",
                "这个文件夹不是 git 检出。",
            ),
        )),
    }
    out.push(heading(k, t("Changes", "变更")));
    if s.diff_stat.files == 0 {
        out.push(empty(
            k,
            "changes",
            t("Working tree is clean", "工作区没有改动"),
            t(
                "Edits the agent makes show up here.",
                "Agent 的修改会显示在这里。",
            ),
        ));
    } else {
        out.push(muted(
            k,
            format!(
                "{} file(s), +{} −{}",
                s.diff_stat.files, s.diff_stat.added, s.diff_stat.removed
            ),
        ));
        for f in s.diff_stat.paths.iter().take(12) {
            out.push(item(
                k,
                format!("gf-{}", f.path),
                f.path.clone(),
                format!("+{} −{}", f.added, f.removed),
                None,
            ));
        }
        out.push(small(
            k,
            t("View changes", "查看变更"),
            ButtonVariant::Outline,
            "view-changes".into(),
            |s| {
                s.mode = Mode::Session;
                s.show_diff = true;
            },
        ));
    }
    out.push(heading(k, t("Worktrees", "Worktree")));
    if s.worktrees.is_empty() {
        out.push(empty(
            k,
            "worktrees",
            t("No linked worktrees", "没有关联的 worktree"),
            t(
                "Start a session in a new worktree from Home.",
                "在首页用新的 worktree 开始一个会话。",
            ),
        ));
    }
    let (agent, dir) = info
        .map(|i| (i.agent_id.clone(), i.directory.clone().unwrap_or_default()))
        .unwrap_or_default();
    for w in &s.worktrees {
        let action = (!w.is_root()).then(|| {
            let (agent, project, worktree) = (agent.clone(), dir.clone(), w.directory.clone());
            small(
                k,
                t("Remove", "移除"),
                ButtonVariant::Ghost,
                format!("rmw-{}", w.directory),
                move |s| {
                    s.worktrees.retain(|x| x.directory != worktree);
                    s.send(Command::RemoveWorktree {
                        agent: agent.clone(),
                        project: project.clone(),
                        worktree: worktree.clone(),
                    });
                },
            )
        });
        out.push(item(
            k,
            format!("wt-{}", w.directory),
            w.directory.clone(),
            w.strategy
                .clone()
                .unwrap_or_else(|| tr(language, "root", "主目录")),
            action,
        ));
    }
    if !dir.is_empty() {
        let (agent, directory) = (agent.clone(), dir.clone());
        out.push(small(
            k,
            t("Rediscover", "重新发现"),
            ButtonVariant::Ghost,
            "wt-refresh".into(),
            move |s| {
                s.send(Command::RefreshWorktrees {
                    agent: agent.clone(),
                    directory: directory.clone(),
                });
            },
        ));
    }
    out
}

fn subagents(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    if s.descendants.is_empty() {
        return vec![empty(
            k,
            "subagents",
            tr_ref(language, "No sub-agents", "没有子 Agent"),
            tr_ref(
                language,
                "Sessions this one delegates to appear here.",
                "这个会话委派出去的会话会显示在这里。",
            ),
        )];
    }
    s.descendants
        .iter()
        .map(|d| {
            let asid = d.asid.0.clone();
            let open = small(
                k,
                tr_ref(language, "Open", "打开"),
                ButtonVariant::Ghost,
                format!("open-{}", d.asid.0),
                move |s| s.open_session(&asid),
            );
            item(
                k,
                format!("sub-{}", d.asid.0),
                title_of(d),
                format!("{} · {:?}", s.agent_name(&d.agent_id), d.status).to_lowercase(),
                Some(open),
            )
        })
        .collect()
}

fn saved(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    if s.saved_permissions.is_empty() {
        return vec![empty(
            k,
            "saved",
            tr_ref(language, "Nothing remembered", "没有记住的决定"),
            tr_ref(
                language,
                "\"Always allow\" answers for this project are kept here.",
                "这个项目的「始终允许」会记在这里。",
            ),
        )];
    }
    let asid = s.selected.clone().unwrap_or_default();
    s.saved_permissions
        .iter()
        .map(|p| {
            let (id, a) = (p.id.clone(), AgentSessionId(asid.clone()));
            let forget = small(
                k,
                tr_ref(language, "Forget", "忘记"),
                ButtonVariant::Ghost,
                format!("fg-{}", p.id),
                move |s| {
                    s.saved_permissions.retain(|x| x.id != id);
                    s.send(Command::ForgetSavedPermission {
                        asid: a.clone(),
                        id: id.clone(),
                    });
                },
            );
            item(
                k,
                format!("sp-{}", p.id),
                p.action.clone(),
                p.resource.clone(),
                Some(forget),
            )
        })
        .collect()
}

fn shells(s: &UiState, k: &Kit) -> Vec<El> {
    let (agent, dir) = s
        .current()
        .map(|i| (i.agent_id.clone(), i.directory.clone()))
        .unwrap_or_default();
    let language = s.cfg.general.language;
    let mut out: Vec<El> = Vec::new();
    if s.shells.is_empty() {
        out.push(empty(
            k,
            "shells",
            tr_ref(language, "No background shells", "没有后台命令"),
            tr_ref(
                language,
                "Long commands the agent parks keep running here.",
                "Agent 放到后台的长命令会在这里继续运行。",
            ),
        ));
    }
    for sh in &s.shells {
        let (agent, id, dir) = (agent.clone(), sh.id.clone(), dir.clone());
        let kill = small(
            k,
            tr_ref(language, "Stop", "停止"),
            ButtonVariant::Ghost,
            format!("kill-{}", sh.id),
            move |s| {
                s.send(Command::KillShell {
                    agent: agent.clone(),
                    id: id.clone(),
                    directory: dir.clone(),
                });
            },
        );
        let detail = match sh.pid {
            Some(pid) => format!("{} · pid {pid}", sh.status),
            None => sh.status.clone(),
        };
        out.push(item(
            k,
            format!("sh-{}", sh.id),
            timeline::clip(&sh.command, 80),
            detail,
            Some(kill),
        ));
    }
    if let Some(asid) = s.selected.clone() {
        out.push(small(
            k,
            tr_ref(language, "Move turn to background", "把当前回合移到后台"),
            ButtonVariant::Outline,
            "background".into(),
            move |s| {
                s.send(Command::BackgroundSession {
                    asid: AgentSessionId(asid.clone()),
                });
            },
        ));
    }
    out
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let asid = s.selected.clone().unwrap_or_default();
    let waiting = s.waiting_perms.get(&asid).map_or(0, Vec::len)
        + s.waiting_forms.get(&asid).map_or(0, Vec::len)
        + s.queue.len();
    let labels: Vec<String> = TABS
        .iter()
        .map(|(label, tab)| {
            let label = tab_label(language, label);
            match tab {
                InspectorTab::Queue if waiting > 0 => format!("{label} {waiting}"),
                InspectorTab::Git if s.diff_stat.files > 0 => {
                    format!("{label} {}", s.diff_stat.files)
                }
                InspectorTab::Shells if !s.shells.is_empty() => {
                    format!("{label} {}", s.shells.len())
                }
                _ => label.to_string(),
            }
        })
        .collect();
    let at = TABS.iter().position(|(_, t)| *t == s.inspector_tab);
    let tabs = SegmentedControl::new(&labels, at)
        .width(None)
        .label(tr_ref(language, "Inspector", "检查器"))
        .on_pick(move |s: &mut UiState, i, _: &mut Tx| {
            s.inspector_tab = TABS[i].1;
            s.refresh_inspector();
        })
        .build(k)
        .key("inspector-tabs");
    let body = match s.inspector_tab {
        InspectorTab::Queue => queue(s, k),
        InspectorTab::Git => git(s, k),
        InspectorTab::Subagents => subagents(s, k),
        InspectorTab::Saved => saved(s, k),
        InspectorTab::Shells => shells(s, k),
    };
    col()
        .key("inspector")
        .w_full()
        .h_full()
        .min_h(0.0)
        .gap(12.0)
        .px(12.0)
        .py(12.0)
        .child(tabs)
        .child(
            col()
                .key("inspector-body")
                .w_full()
                .gap(8.0)
                .children(body)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .grow(1.0)
                .min_h(0.0)
                .scroll_bar(k),
        )
}
