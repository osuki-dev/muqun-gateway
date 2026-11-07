//! The right-hand inspector: Queue, Git (with worktrees), Subagents, Saved, Shells.

use muqun_gateway::agents::AgentSessionId;
use rune::{col, row, text, El, ScrollSpec};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, ScrollBar};

use super::{muted, timeline, title_of, InspectorTab, Mode, UiState};
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
        .py(4.0)
        .child(copy);
    if let Some(action) = action {
        line = line.child(action);
    }
    line
}

fn heading(k: &Kit, label: &str) -> El {
    text(label.to_string())
        .font(k.ty.small_medium)
        .color(k.c.text_faint)
        .key(format!("h-{label}"))
}

fn queue(s: &UiState, k: &Kit) -> Vec<El> {
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
        out.push(heading(k, "Waiting on you"));
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
    out.push(heading(k, "Queued prompts"));
    if s.queue.is_empty() {
        out.push(muted(k, "Nothing is waiting for the agent."));
    }
    for q in &s.queue {
        let (id, a) = (q.id.clone(), AgentSessionId(asid.clone()));
        let steer = q.steer;
        let (id2, a2) = (id.clone(), a.clone());
        let action = row()
            .gap(4.0)
            .child(small(
                k,
                if steer { "Queue" } else { "Steer" },
                ButtonVariant::Ghost,
                format!("dl-{id}"),
                move |s| {
                    s.send(Command::Compose(ComposeCommand::InboxDelivery {
                        asid: a.clone(),
                        item: id.clone(),
                        steer: !steer,
                    }));
                },
            ))
            .child(small(
                k,
                "Cancel",
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
        let detail = format!(
            "{}{}",
            if q.steer { "steer" } else { "queue" },
            if q.attachments > 0 {
                format!(" · {} file(s)", q.attachments)
            } else {
                String::new()
            }
        );
        out.push(item(
            k,
            format!("q-{}", q.id),
            timeline::clip(&q.text, 80),
            detail,
            Some(action),
        ));
    }
    out
}

fn git(s: &UiState, k: &Kit) -> Vec<El> {
    let mut out = vec![heading(k, "Branch")];
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
        None => out.push(muted(k, "No repository information.")),
    }
    out.push(heading(k, "Changes"));
    if s.diff_stat.files == 0 {
        out.push(muted(k, "Working tree is clean."));
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
            "View changes",
            ButtonVariant::Outline,
            "view-changes".into(),
            |s| {
                s.mode = Mode::Session;
                s.show_diff = true;
            },
        ));
    }
    out.push(heading(k, "Worktrees"));
    if s.worktrees.is_empty() {
        out.push(muted(k, "No linked worktrees."));
    }
    let (agent, dir) = info
        .map(|i| (i.agent_id.clone(), i.directory.clone().unwrap_or_default()))
        .unwrap_or_default();
    for w in &s.worktrees {
        let action = (!w.is_root()).then(|| {
            let (agent, project, worktree) = (agent.clone(), dir.clone(), w.directory.clone());
            small(
                k,
                "Remove",
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
            w.strategy.clone().unwrap_or_else(|| "root".into()),
            action,
        ));
    }
    if !dir.is_empty() {
        let (agent, directory) = (agent.clone(), dir.clone());
        out.push(small(
            k,
            "Rediscover",
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
    if s.descendants.is_empty() {
        return vec![muted(k, "This session has no sub-agents.")];
    }
    s.descendants
        .iter()
        .map(|d| {
            let asid = d.asid.0.clone();
            let open = small(
                k,
                "Open",
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
    if s.saved_permissions.is_empty() {
        return vec![muted(k, "No remembered \"always allow\" decisions.")];
    }
    let asid = s.selected.clone().unwrap_or_default();
    s.saved_permissions
        .iter()
        .map(|p| {
            let (id, a) = (p.id.clone(), AgentSessionId(asid.clone()));
            let forget = small(
                k,
                "Forget",
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
    let mut out: Vec<El> = Vec::new();
    if s.shells.is_empty() {
        out.push(muted(k, "No background shells."));
    }
    for sh in &s.shells {
        let (agent, id, dir) = (agent.clone(), sh.id.clone(), dir.clone());
        let kill = small(
            k,
            "Stop",
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
            "Move turn to background",
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
    let mut tabs = row().key("inspector-tabs").w_full().gap(4.0).wrap();
    for (label, tab) in TABS {
        let variant = if s.inspector_tab == tab {
            ButtonVariant::Contrast
        } else {
            ButtonVariant::Ghost
        };
        tabs = tabs.child(small(k, label, variant, format!("tab-{label}"), move |s| {
            s.inspector_tab = tab;
            s.refresh_inspector();
        }));
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
