//! The sidebar's Running row, above Inbox: what is executing right now.
//! Pressing it opens a short list of the working sessions and running
//! tasks, the ones waiting on the owner first; an entry opens its session.

use muqun_gateway::agents::AgentSessionInfo;
use rune::{col, el, icon, row, El, ScrollSpec, Tx};
use rune_kit::glyphs::CHEVRON_DOWN;
use rune_kit::shell::SidebarRow;
use rune_kit::Kit;

use super::copy::{localize, tr, tr_ref};
use super::{now_ms, title_of, Activity, Mode, UiState};
use crate::config::{ConfigEdit, Language};
use crate::hub::TaskRow;

/// The open list's tallest, before it scrolls: about five entries.
const LIST_MAX_H: f32 = 212.0;
const ENTRY_H: f32 = 40.0;

/// One thing executing.
pub enum Entry<'a> {
    /// An agent session working or waiting, with the task it runs for.
    Session {
        info: &'a AgentSessionInfo,
        task: Option<&'a TaskRow>,
    },
    /// A task with no session listed here: a command, a terminal or
    /// another computer's delegation.
    Task(&'a TaskRow),
}

impl Entry<'_> {
    /// It waits on the owner: a permission or form, or a paused task.
    pub fn waiting(&self, s: &UiState) -> bool {
        match self {
            Entry::Session { info, .. } => s.activity(info) == Activity::NeedsYou,
            Entry::Task(t) => t.state == "waiting",
        }
    }

    /// When it started, as well as the window knows.
    fn since(&self, s: &UiState) -> u64 {
        match self {
            Entry::Session { info, task } => task
                .map(|t| t.started_ms)
                .filter(|&ms| ms > 0)
                .or_else(|| s.busy_since.get(&info.asid.0).copied())
                .unwrap_or(info.updated_ms),
            Entry::Task(t) => {
                if t.started_ms > 0 {
                    t.started_ms
                } else {
                    t.updated_ms
                }
            }
        }
    }
}

/// A task still executing (queued, starting, working or paused on the owner).
fn task_live(t: &TaskRow) -> bool {
    matches!(
        t.state.as_str(),
        "queued" | "dispatching" | "working" | "waiting"
    )
}

/// Everything executing: waiting ones first, then the newest started.
pub fn entries(s: &UiState) -> Vec<Entry<'_>> {
    let mut out: Vec<Entry<'_>> = s
        .sessions
        .iter()
        .filter(|i| matches!(s.activity(i), Activity::Working | Activity::NeedsYou))
        .map(|info| Entry::Session {
            info,
            task: s
                .tasks
                .iter()
                .find(|t| task_live(t) && t.asid.as_deref() == Some(info.asid.0.as_str())),
        })
        .collect();
    for t in s.tasks.iter().filter(|t| task_live(t)) {
        let shown = t.asid.as_deref().is_some_and(|asid| {
            out.iter()
                .any(|e| matches!(e, Entry::Session { info, .. } if info.asid.0 == asid))
        });
        if !shown {
            out.push(Entry::Task(t));
        }
    }
    out.sort_by_key(|e| (!e.waiting(s), std::cmp::Reverse(e.since(s))));
    out
}

/// How long something has run: "<1m", "12m", "1h 05m", "2d".
pub fn elapsed(ms: u64, language: Language) -> String {
    let mins = ms / 60_000;
    let zh = language.is_zh();
    let text = match mins {
        0 => tr(language, "<1m", "<1 分"),
        1..=59 => format!("{mins}{}", if zh { " 分" } else { "m" }),
        60..=1439 => {
            let (h, m) = (mins / 60, mins % 60);
            if zh {
                format!("{h} 小时 {m} 分")
            } else {
                format!("{h}h {m:02}m")
            }
        }
        _ => format!("{}{}", mins / 1440, if zh { " 天" } else { "d" }),
    };
    localize(language, text)
}

fn open_entry(s: &mut UiState, asid: Option<String>) {
    match asid {
        Some(asid) => s.open_session(&asid),
        None => s.go(Mode::Tasks),
    }
}

/// One entry's row: who, what, what it is doing, for how long.
fn entry_row(s: &UiState, k: &Kit, e: &Entry<'_>, now: u64) -> El {
    let language = s.cfg.general.language;
    let look = s.look();
    let waiting = e.waiting(s);
    let (key, title, agent, asid, mut step) = match e {
        Entry::Session { info, task } => {
            let step = task.and_then(|t| t.progress.clone()).or_else(|| {
                UiState::folder_name(info)
                    .map(|f| format!("{} · {f}", s.agent_name(&info.agent_id)))
            });
            (
                info.asid.0.clone(),
                title_of(info),
                Some(info.agent_id.clone()),
                Some(info.asid.0.clone()),
                step.unwrap_or_else(|| s.agent_name(&info.agent_id)),
            )
        }
        Entry::Task(t) => {
            let mut parts = Vec::new();
            if let Some(m) = &t.machine {
                parts.push(localize(
                    language,
                    tr(language, &format!("on {m}"), &format!("在 {m}")),
                ));
            } else if let Some(a) = &t.agent_id {
                parts.push(s.agent_name(a));
            }
            match t.progress.as_deref().filter(|p| !p.is_empty()) {
                Some(p) => parts.push(p.to_string()),
                None if t.state != "working" && t.state != "waiting" => {
                    parts.push(tr(language, "Starting…", "启动中…"));
                }
                None => {}
            }
            if parts.is_empty() {
                parts.push(if t.kind == "command" {
                    tr(language, "Command", "命令")
                } else {
                    tr(language, "Task", "任务")
                });
            }
            (
                format!("task-{}", t.id),
                t.title.clone(),
                t.agent_id.clone(),
                t.asid.clone(),
                parts.join(" · "),
            )
        }
    };
    if waiting {
        step = tr(language, "Needs you", "等你处理");
    }
    let lit = if waiting {
        k.c.warning.icon
    } else {
        agent
            .as_deref()
            .map_or(k.c.text_accent, |a| super::agents::color(a, s.dark))
    };
    let mark = match &agent {
        Some(a) => super::agents::mark(a, 14.0, s.dark),
        None => icon(&super::icons::TASK, 14.0).color(k.c.icon_muted),
    };
    let dot = el()
        .size(7.0, 7.0)
        .radius(3.5)
        .fill(lit)
        .border(1.5, k.c.bg_deep)
        .absolute(10.0, 10.0);
    let avatar = el()
        .size(16.0, 16.0)
        .center()
        .child(mark)
        .child(look.pulse(dot).a11y_hidden());
    let mut r = SidebarRow::new(title)
        .depth(1)
        .height(ENTRY_H)
        .avatar(avatar)
        .subtitle(step)
        .detail(elapsed(now.saturating_sub(e.since(s)), language))
        .selected(asid.is_some() && s.mode == Mode::Session && s.selected == asid);
    if waiting {
        r = r.subtitle_color(k.c.warning.fg);
    }
    r.on_select(move |s: &mut UiState, _: &mut Tx| open_entry(s, asid.clone()))
        .build(k)
        .key(format!("running-{key}"))
}

/// The Running row and, when open, its list. Nothing while nothing runs.
pub fn view(s: &UiState, k: &Kit) -> Option<El> {
    let list = entries(s);
    if list.is_empty() {
        return None;
    }
    let language = s.cfg.general.language;
    let look = s.look();
    let open = s.cfg.ui.running_open;
    let waiting = list.iter().filter(|e| e.waiting(s)).count();
    // The lead turns while anything runs (still under reduce motion); a
    // dot in the warning colour says something waits on the owner.
    let lead = if look.motion {
        rune_kit::Spinner::new()
            .size(13.0)
            .color(k.c.text_accent)
            .build(k)
    } else {
        icon(&super::icons::RUNNING, 14.0).color(k.c.text_accent)
    };
    let chevron = icon(&CHEVRON_DOWN, 12.0)
        .color(k.c.icon_muted)
        .rotate(if open {
            0.0
        } else {
            -std::f32::consts::FRAC_PI_2
        })
        .rotate_motion(if look.motion {
            rune::Anim::from(k.motion.control)
        } else {
            rune::Anim::INSTANT
        });
    let mut trailing = row().gap(8.0).center_y().a11y_hidden();
    if waiting > 0 {
        let warn = k.c.warning.icon;
        trailing = trailing.child(look.pulse(el().size(6.0, 6.0).radius(3.0).fill(warn)));
    }
    let trailing = trailing.child(chevron);
    let words = tr_ref(language, "Running", "运行中");
    let head = SidebarRow::new(words)
        .avatar(lead)
        .badge(list.len().to_string())
        .trailing(trailing)
        .on_select(move |s: &mut UiState, _: &mut Tx| {
            let open = !s.cfg.ui.running_open;
            s.edit(ConfigEdit::RunningOpen(open));
        })
        .build(k)
        .key("nav-running")
        .expanded(open)
        .help(if waiting > 0 {
            localize(
                language,
                tr(
                    language,
                    &format!("{} running, {waiting} waiting on you", list.len()),
                    &format!("{} 项运行中，{waiting} 项等你处理", list.len()),
                ),
            )
        } else {
            tr(language, "What is executing now", "正在执行的内容")
        });
    let mut block = col().key("running").w_full().child(head);
    if open {
        let now = now_ms();
        block = block.child(
            col()
                .key("running-scroll")
                .w_full()
                .max_h(LIST_MAX_H)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_edge_fade(10.0)
                .child(
                    col()
                        .key("running-list")
                        .role("list")
                        .label(words)
                        .w_full()
                        .children(list.iter().map(|e| entry_row(s, k, e, now))),
                ),
        );
    }
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_reads_at_a_glance() {
        for (ms, en) in [
            (20_000, "<1m"),
            (12 * 60_000, "12m"),
            (65 * 60_000, "1h 05m"),
            (50 * 3_600_000, "2d"),
        ] {
            assert_eq!(elapsed(ms, Language::En), en);
        }
        assert_eq!(elapsed(65 * 60_000, Language::ZhCn), "1 小时 5 分");
    }
}
