//! The right-hand inspector: Queue, Git (with worktrees), Subagents, Saved, Shells.

use std::path::PathBuf;

use muqun_gateway::agents::AgentSessionId;
use rune::{col, el, row, text, El, ScrollSpec};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, ScrollBar};

use super::copy::{tr, tr_ref};
use super::{muted, timeline, title_of, InspectorTab, Mode, UiState};
use crate::config::Language;
use crate::model::commands::Command;
use crate::model::compose::ComposeCommand;

const TABS: [(&str, InspectorTab); 5] = [
    ("Queue", InspectorTab::Queue),
    ("Git", InspectorTab::Git),
    ("Sub-agents", InspectorTab::Subagents),
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
        "Sub-agents" => tr_ref(language, "Sub-agents", "子 Agent"),
        "Saved" => tr_ref(language, "Saved", "已保存"),
        "Shells" => tr_ref(language, "Shells", "后台命令"),
        other => other,
    }
}

/// The tab strip's height: the session toolbar's, so the two read as one line.
const TOOLBAR_H: f32 = rune_kit::shell::TOOLBAR_HEIGHT;

/// One inspector tab, drawn like the toolbar's own controls: ghost until
/// hovered, the pressed tint when chosen, a faint count after the word.
fn tab_button(k: &Kit, label: &str, count: Option<usize>, tab: InspectorTab, on: bool) -> El {
    let ink = if on { k.c.text_base } else { k.c.text_muted };
    let mut b = row()
        .key(format!("tab-{tab:?}"))
        .h(28.0)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .shrink()
        .min_w(0.0)
        .radius(k.radius.md)
        .role("tab")
        .label(label.to_string())
        .focusable(true)
        .focus_selected(on)
        .child(
            text(label.to_string())
                .font(if on { k.ty.ui_medium } else { k.ty.ui })
                .color(ink)
                .truncate()
                .shrink()
                .a11y_hidden(),
        );
    if let Some(n) = count {
        b = b.child(
            text(n.to_string())
                .font(k.ty.small)
                .color(k.c.text_faint)
                .a11y_hidden(),
        );
    }
    if on {
        b = b.fill(k.c.pressed);
    }
    k.interactive(b).on_press(move |s: &mut UiState| {
        s.inspector_tab = tab;
        s.refresh_inspector();
    })
}

/// A quiet empty state: a word and a line under it, no box.
fn empty(k: &Kit, key: &str, title: &str, detail: &str) -> El {
    col()
        .key(format!("empty-{key}"))
        .w_full()
        .gap(2.0)
        .px(4.0)
        .py(8.0)
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

    let git_dir = cwd
        .as_ref()
        .and_then(|c| crate::model::compose::git_dir(std::path::Path::new(c)));
    let remote_url = git_dir
        .as_ref()
        .and_then(|g| crate::model::compose::parse_git_remote_url(g));
    let web_url = remote_url
        .as_deref()
        .and_then(crate::model::compose::parse_repo_web_url);

    match cwd.as_ref().and_then(|c| s.branches.get(c)) {
        Some(b) => {
            let cur_branch = b.current.clone().unwrap_or_else(|| "detached".into());
            out.push(
                Badge::new(cur_branch.clone())
                    .variant(BadgeVariant::Accent)
                    .build(k),
            );
            if !b.local.is_empty() {
                out.push(muted(k, format!("{} local branch(es)", b.local.len())));
            }

            let cwd_path = cwd.as_ref().map(PathBuf::from);
            out.push(small(
                k,
                t("Open in Terminal", "在终端中打开 Git"),
                ButtonVariant::Ghost,
                "git-open-term".into(),
                move |s| {
                    let dir = cwd_path.clone().unwrap_or_else(|| s.terminal_cwd());
                    s.term_new_tab_in(dir, Some("git status\n".into()));
                },
            ));

            if let Some(repo_url) = &web_url {
                out.push(heading(
                    k,
                    t("Remote & Pull Request", "远程与 Pull Request"),
                ));
                let host_name = repo_url
                    .strip_prefix("https://")
                    .or_else(|| repo_url.strip_prefix("http://"))
                    .unwrap_or(repo_url);
                out.push(muted(k, format!("origin: {host_name}")));

                if cur_branch != "main" && cur_branch != "master" && cur_branch != "detached" {
                    let pr_url = format!("{repo_url}/pull/new/{cur_branch}");
                    out.push(small(
                        k,
                        t("Create Pull Request", "创建 Pull Request"),
                        ButtonVariant::Outline,
                        "git-create-pr".into(),
                        move |_| {
                            crate::platform::shell::open_url(&pr_url);
                        },
                    ));
                } else {
                    let pr_list_url = format!("{repo_url}/pulls");
                    out.push(small(
                        k,
                        t("View Pull Requests", "查看 Pull Request 列表"),
                        ButtonVariant::Outline,
                        "git-view-prs".into(),
                        move |_| {
                            crate::platform::shell::open_url(&pr_list_url);
                        },
                    ));
                }

                let repo_view_url = repo_url.clone();
                out.push(small(
                    k,
                    t("View on GitHub", "在 GitHub 上查看仓库"),
                    ButtonVariant::Ghost,
                    "git-view-repo".into(),
                    move |_| {
                        crate::platform::shell::open_url(&repo_view_url);
                    },
                ));
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
        for (i, f) in s.diff_stat.paths.iter().take(12).enumerate() {
            let p = f.path.clone();
            let asid = s.selected.clone().unwrap_or_default();
            let diff_btn = small(
                k,
                t("Diff", "差异"),
                ButtonVariant::Ghost,
                format!("open-diff-item-{i}"),
                move |s| {
                    if !asid.is_empty() {
                        s.send(Command::OpenDiff {
                            asid: AgentSessionId(asid.clone()),
                            file: Some(p.clone()),
                        });
                    }
                    s.mode = Mode::Session;
                    s.show_diff = true;
                },
            );
            let discard_path = f.path.clone();
            let discard_btn = small(
                k,
                crate::ui::copy::t_ref(s.cfg.general.language, "Discard"),
                ButtonVariant::Ghost,
                format!("discard-item-{i}"),
                move |s| {
                    let cwd = s
                        .current()
                        .and_then(|info| info.directory.clone())
                        .unwrap_or_else(|| s.terminal_cwd().display().to_string());
                    if !cwd.is_empty() {
                        s.send(Command::Compose(ComposeCommand::DiscardFile {
                            cwd: cwd.clone(),
                            path: discard_path.clone(),
                        }));
                        // Refresh diff and inspector
                        if let Some(asid) = s.selected.clone() {
                            s.send(Command::RefreshDiff(AgentSessionId(asid)));
                        }
                        s.refresh_inspector();
                        s.toast(
                            crate::ui::copy::t(s.cfg.general.language, "Reverted file changes"),
                            None,
                        );
                    }
                },
            );
            let actions = row().gap(4.0).center_y().child(diff_btn).child(discard_btn);
            out.push(item(
                k,
                format!("gf-{}", f.path),
                f.path.clone(),
                format!("+{} −{}", f.added, f.removed),
                Some(actions),
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
        let mut actions = row().gap(6.0).center_y();
        let dir_path = PathBuf::from(&w.directory);
        actions = actions.child(small(
            k,
            t("Terminal", "终端"),
            ButtonVariant::Ghost,
            format!("term-wt-{}", w.directory),
            move |s| {
                s.term_new_tab_in(dir_path.clone(), None);
            },
        ));
        if !w.is_root() {
            let (agent, project, worktree) = (agent.clone(), dir.clone(), w.directory.clone());
            actions = actions.child(small(
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
            ));
        }
        out.push(item(
            k,
            format!("wt-{}", w.directory),
            w.directory.clone(),
            w.strategy
                .clone()
                .unwrap_or_else(|| tr(language, "root", "主目录")),
            Some(actions),
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
    let mut out = Vec::new();
    if s.descendants.len() > 1 {
        let sub_ids: Vec<String> = s
            .descendants
            .iter()
            .take(super::MAX_TILES.saturating_sub(1))
            .map(|d| d.asid.0.clone())
            .collect();
        let tile_all_btn = small(
            k,
            tr_ref(language, "Tile all", "全部同屏"),
            ButtonVariant::Outline,
            "tile-all-subagents".into(),
            move |s| {
                for id in &sub_ids {
                    s.open_in_tile(id);
                }
            },
        );
        let header_row = row()
            .key("subagents-header-actions")
            .w_full()
            .center_y()
            .px(4.0)
            .py(2.0)
            .child(
                text(format!(
                    "{} {}",
                    s.descendants.len(),
                    tr_ref(language, "sub-agents", "个子 Agent")
                ))
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
            )
            .child(el().grow(1.0))
            .child(tile_all_btn);
        out.push(header_row);
    }
    out.extend(s.descendants.iter().map(|d| {
        let asid = d.asid.0.clone();
        let asid_split = asid.clone();
        let in_tile = s.tile_of(&asid).is_some();
        let split_label = if in_tile {
            tr_ref(language, "Tiled", "已分屏")
        } else {
            tr_ref(language, "Split", "分屏")
        };
        let split_btn = small(
            k,
            split_label,
            if in_tile {
                ButtonVariant::Outline
            } else {
                ButtonVariant::Ghost
            },
            format!("split-{}", d.asid.0),
            move |s| s.open_in_tile(&asid_split),
        );
        let open_btn = small(
            k,
            tr_ref(language, "Open", "打开"),
            ButtonVariant::Ghost,
            format!("open-{}", d.asid.0),
            move |s| s.open_session(&asid),
        );
        let actions = row().gap(6.0).center_y().child(split_btn).child(open_btn);

        use muqun_gateway::agents::domain::session::AgentSessionStatus;
        let status_text = match d.status {
            AgentSessionStatus::Busy => tr_ref(language, "Running", "运行中"),
            AgentSessionStatus::Idle => tr_ref(language, "Idle", "空闲"),
            AgentSessionStatus::Retry => tr_ref(language, "Retrying", "重试中"),
            AgentSessionStatus::Failed => tr_ref(language, "Failed", "失败"),
            AgentSessionStatus::Interrupted => tr_ref(language, "Stopped", "已停止"),
            AgentSessionStatus::Unknown => tr_ref(language, "Unknown", "未知"),
        };

        item(
            k,
            format!("sub-{}", d.asid.0),
            title_of(d),
            format!("{} · {}", s.agent_name(&d.agent_id), status_text),
            Some(actions),
        )
    }));
    out
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
            ButtonVariant::Ghost,
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
    let count = |tab: InspectorTab| match tab {
        InspectorTab::Queue if waiting > 0 => Some(waiting),
        InspectorTab::Git if s.diff_stat.files > 0 => Some(s.diff_stat.files),
        InspectorTab::Shells if !s.shells.is_empty() => Some(s.shells.len()),
        _ => None,
    };
    let mut strip = row()
        .key("inspector-tabs")
        .role("tablist")
        .label(tr_ref(language, "Inspector", "检查器"))
        .w_full()
        .h(TOOLBAR_H)
        .px(8.0)
        .gap(2.0)
        .center_y()
        .focus_group();
    for (label, tab) in TABS {
        strip = strip.child(tab_button(
            k,
            tab_label(language, label),
            count(tab),
            tab,
            s.inspector_tab == tab,
        ));
    }
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
        .child(strip)
        .child(
            col()
                .key("inspector-body")
                .w_full()
                .gap(8.0)
                .px(12.0)
                .pt(4.0)
                .pb(12.0)
                .children(body)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .grow(1.0)
                .min_h(0.0)
                .scroll_bar(k),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::AgentSessionInfo;

    #[test]
    fn subagents_renders_items_and_tile_all() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let sub1: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "sub-1",
            "agent_id": "opencode",
            "title": "Fix bug in parser",
            "status": "busy",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        let sub2: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "sub-2",
            "agent_id": "deepseek",
            "title": "Add tests for API",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        s.descendants = vec![sub1, sub2];
        let k = Kit::neutral(s.dark);
        let els = subagents(&s, &k);
        assert_eq!(els.len(), 3); // 1 header banner with "Tile all" + 2 subagent rows
    }
}
