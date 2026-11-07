//! The window: sidebar, main pane, inspector and toolbar in one `SplitView`.

use std::sync::Arc;

use rune::{col, el, row, text, Cx, El, SurfaceFit};
use rune_kit::shell::{SidebarToggle, SplitView, Toolbar, INSPECTOR};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Card, ControlSize, IconButton, IconButtonVariant,
    Kit,
};

use super::copy::{tr, tr_ref};
use super::{art, composer, home, inbox, inspector, sessions, settings, tasks, timeline};
use super::{Mode, UiState};
use crate::branding::{APP_MARK_PNG, APP_NAME};
use crate::config::Language;
use crate::model::commands::Command;

pub fn view(s: &UiState, cx: &mut Cx<UiState>, k: &Kit, layer: &mut Vec<El>) -> El {
    let page = match s.mode {
        Mode::Home => home::view(s, k, layer),
        Mode::Session => session(s, k, layer),
        Mode::Inbox => inbox::view(s, k),
        Mode::Tasks => tasks::view(s, k),
        Mode::Settings => settings::view(s, k),
    };
    let mut main = col().key("main").w_full().h_full().min_h(0.0);
    if let Some(alert) = alert(s, k) {
        main = main.child(alert);
    }
    let main = main.child(page.grow(1.0).min_h(0.0));
    let mut split = SplitView::new(main)
        .sidebar(&s.sidebar, super::sidebar_pane, sessions::view(s, k, layer))
        .sidebar_leading(home_brand(s, k))
        .toolbar(toolbar(s, k));
    if s.right_open && s.mode == Mode::Session {
        split = split.inspector(&s.right, super::right_pane, inspector::view(s, k));
    }
    split
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(8.0)
        .toolbar_in_panel()
        .sidebar_toggle(SidebarToggle::new().size(ControlSize::Large).in_header())
        .build(k)
}

/// Mark + name in the sidebar title bar; press returns to Home.
fn home_brand(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let mark = match s.images.as_ref() {
        Some(images) => art::picture(images, Arc::<[u8]>::from(APP_MARK_PNG), SurfaceFit::Fit)
            .key("home-mark")
            .w(22.0)
            .h(22.0)
            .radius(5.0)
            .clip_rounded(5.0),
        None => el()
            .key("home-mark")
            .w(22.0)
            .h(22.0)
            .radius(5.0)
            .fill(k.c.bg_accent),
    };
    row()
        .key("home-brand")
        .gap(8.0)
        .center_y()
        .role("button")
        .label(tr_ref(language, "Home", "首页"))
        .child(mark)
        .child(
            text(APP_NAME)
                .key("home-name")
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        )
        .on_press(|s: &mut UiState| s.go(Mode::Home))
}

fn title(s: &UiState) -> String {
    let language = s.cfg.general.language;
    match s.mode {
        Mode::Home => tr(language, "New session", "新建会话"),
        Mode::Inbox => tr(language, "Inbox", "收件箱"),
        Mode::Tasks => tr(language, "Tasks", "任务"),
        Mode::Settings => tr(language, "Settings", "设置"),
        Mode::Session => s
            .current()
            .map_or_else(|| s.status.clone(), super::title_of),
    }
}

fn hub_badge_label(language: Language, raw: &str) -> String {
    match raw {
        "" => tr(language, "Connecting…", "连接中…"),
        "local gateway" => tr(language, "Local gateway", "本机 Gateway"),
        "standalone" => tr(language, "Standalone", "独立运行"),
        other => other.to_string(),
    }
}

fn status_interesting(status: &str) -> bool {
    let s = status.trim();
    !(s.is_empty()
        || s == "Starting…"
        || s == "Sent"
        || s.starts_with("Opening ")
        || s.ends_with(" messages")
        || s.ends_with(" agent(s) discovered"))
}

fn toolbar(s: &UiState, k: &Kit) -> Toolbar {
    let language = s.cfg.general.language;
    let hub = hub_badge_label(language, &s.hub_label);
    let mut bar = Toolbar::new().title(title(s));
    if status_interesting(&s.status) {
        bar = bar.trailing(
            Badge::new(s.status.clone())
                .variant(BadgeVariant::Subtle)
                .build(k)
                .key("status"),
        );
    }
    if s.mode == Mode::Session {
        if s.diff_stat.files > 0 {
            let label = format!(
                "{} {} +{} −{}",
                tr_ref(language, "Changes", "变更"),
                s.diff_stat.files,
                s.diff_stat.added,
                s.diff_stat.removed
            );
            bar = bar.trailing(
                Button::new(label)
                    .variant(if s.show_diff {
                        ButtonVariant::Contrast
                    } else {
                        ButtonVariant::Ghost
                    })
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| match s.selected.clone() {
                        Some(asid) => s.send(Command::OpenDiff {
                            asid: muqun_gateway::agents::AgentSessionId(asid),
                            file: None,
                        }),
                        None => s.show_diff = !s.show_diff,
                    })
                    .build(k)
                    .key("changes"),
            );
        }
        bar = bar.trailing(
            IconButton::new(&INSPECTOR)
                .variant(if s.right_open {
                    IconButtonVariant::Contrast
                } else {
                    IconButtonVariant::Ghost
                })
                .label(tr_ref(language, "Inspector", "检查器"))
                .on_press(|s: &mut UiState| s.toggle_inspector())
                .build(k)
                .key("inspector-toggle"),
        );
    }
    if s.mode != Mode::Settings {
        bar = bar.trailing(
            Button::new(tr_ref(language, "Settings", "设置"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.go(Mode::Settings))
                .build(k)
                .key("goto-settings"),
        );
    }
    bar.trailing(
        Badge::new(hub)
            .variant(BadgeVariant::Subtle)
            .build(k)
            .key("hub"),
    )
}

/// An error from the core: the message, what to check, and a way to dismiss it.
fn alert(s: &UiState, k: &Kit) -> Option<El> {
    let language = s.cfg.general.language;
    let err = s.error.as_ref()?;
    let mut card = Card::new()
        .accent(k.c.danger.icon)
        .title(err.message.clone(), None)
        .action(
            Button::new(tr_ref(language, "Dismiss", "关闭"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.error = None)
                .build(k)
                .key("dismiss-error"),
        );
    if let Some(detail) = &err.detail {
        card = card.description(detail.clone());
    }
    if err.unreachable {
        card = card.action(
            Button::new(tr_ref(language, "Retry", "重试"))
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.send(Command::Refresh))
                .build(k)
                .key("retry-error"),
        );
    }
    Some(
        col()
            .key("alert")
            .w_full()
            .px(16.0)
            .pt(8.0)
            .child(card.build(k)),
    )
}

fn session(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let body = if s.show_diff {
        timeline::changes(s, k)
    } else {
        timeline::view(s, k)
    };
    let mut page = col().key("session").w_full().min_h(0.0);
    if s.revert_staged {
        page = page.child(revert_banner(s, k));
    }
    page = page
        .child(session_actions(s, k))
        .child(body)
        .child(composer::view(s, k, layer));
    page
}

fn revert_banner(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    Card::new()
        .accent(k.c.warning.icon)
        .title(tr_ref(language, "Rollback staged", "已暂存回滚"), None)
        .description(tr(
            language,
            "Confirm to restore files to the marked message, or cancel.",
            "确认后会把文件恢复到标记的消息，或取消。",
        ))
        .action(
            Button::new(tr_ref(language, "Confirm", "确认"))
                .variant(ButtonVariant::Submit)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| {
                    if let Some(asid) = s.selected.clone() {
                        s.send(Command::CommitRevert {
                            asid: muqun_gateway::agents::AgentSessionId(asid),
                        });
                    }
                })
                .build(k)
                .key("revert-commit"),
        )
        .action(
            Button::new(tr_ref(language, "Cancel", "取消"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| {
                    if let Some(asid) = s.selected.clone() {
                        s.send(Command::ClearRevert {
                            asid: muqun_gateway::agents::AgentSessionId(asid),
                        });
                    }
                })
                .build(k)
                .key("revert-clear"),
        )
        .build(k)
        .key("revert-banner")
        .mx(16.0)
        .mt(8.0)
}

fn session_actions(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let asid = s.selected.clone().unwrap_or_default();
    let busy = s.busy();
    row()
        .key("session-actions")
        .w_full()
        .px(16.0)
        .pt(8.0)
        .gap(8.0)
        .center_y()
        .child(
            Button::new(tr_ref(language, "Fork", "分叉"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .disabled(asid.is_empty())
                .on_press({
                    let asid = asid.clone();
                    move |s: &mut UiState| {
                        s.send(Command::ForkSession {
                            asid: muqun_gateway::agents::AgentSessionId(asid.clone()),
                            message_id: None,
                        });
                    }
                })
                .build(k)
                .key("fork"),
        )
        .child(
            Button::new(tr_ref(language, "Compact", "压缩"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .disabled(asid.is_empty() || busy)
                .on_press({
                    let asid = asid.clone();
                    move |s: &mut UiState| {
                        s.send(Command::CompactSession {
                            asid: muqun_gateway::agents::AgentSessionId(asid.clone()),
                        });
                    }
                })
                .build(k)
                .key("compact"),
        )
        .child(
            Button::new(tr_ref(language, "Delete", "删除"))
                .variant(ButtonVariant::Danger)
                .control_size(ControlSize::Small)
                .disabled(asid.is_empty())
                .on_press({
                    let asid = asid.clone();
                    move |s: &mut UiState| {
                        s.send(Command::DeleteSession(
                            muqun_gateway::agents::AgentSessionId(asid.clone()),
                        ));
                    }
                })
                .build(k)
                .key("delete"),
        )
        .child(
            text(if busy {
                tr(language, "Agent is working…", "Agent 正在工作…")
            } else {
                String::new()
            })
            .font(k.ty.small)
            .color(k.c.text_muted)
            .key("session-busy"),
        )
}
