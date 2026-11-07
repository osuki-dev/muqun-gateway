//! The window: sidebar, main pane, inspector and toolbar in one `SplitView`.

use rune::{col, Cx, El};
use rune_kit::shell::{SidebarToggle, SplitView, Toolbar, INSPECTOR};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Card, ControlSize, IconButton, IconButtonVariant,
    Kit,
};

use super::{composer, home, inbox, inspector, sessions, settings, tasks, timeline};
use super::{Mode, UiState};
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

fn title(s: &UiState) -> String {
    match s.mode {
        Mode::Home => "New session".into(),
        Mode::Inbox => "Inbox".into(),
        Mode::Tasks => "Tasks".into(),
        Mode::Settings => "Settings".into(),
        Mode::Session => s
            .current()
            .map_or_else(|| s.status.clone(), super::title_of),
    }
}

fn toolbar(s: &UiState, k: &Kit) -> Toolbar {
    let hub = if s.hub_label.is_empty() {
        "hub…".to_string()
    } else {
        s.hub_label.clone()
    };
    let mut bar = Toolbar::new().title(title(s));
    if s.mode == Mode::Session {
        if s.diff_stat.files > 0 {
            let label = format!(
                "Changes {} +{} −{}",
                s.diff_stat.files, s.diff_stat.added, s.diff_stat.removed
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
                        // The core answers with the toggle (`UiAction::ToggleDiffPanel`).
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
                .label("Inspector")
                .on_press(|s: &mut UiState| s.toggle_inspector())
                .build(k)
                .key("inspector-toggle"),
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
    let err = s.error.as_ref()?;
    let mut card = Card::new()
        .accent(k.c.danger.icon)
        .title(err.message.clone(), None)
        .action(
            Button::new("Dismiss")
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
            Button::new("Retry")
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
    let _ = s;
    Card::new()
        .accent(k.c.warning.icon)
        .title("Rollback staged", None)
        .description("Confirm to restore files to the marked message, or cancel.")
        .action(
            Button::new("Confirm")
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
            Button::new("Cancel")
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
    let asid = s.selected.clone().unwrap_or_default();
    rune::row()
        .key("session-actions")
        .w_full()
        .px(16.0)
        .pt(8.0)
        .gap(8.0)
        .child(
            Button::new("Fork")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
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
            Button::new("Compact")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
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
            Button::new("Delete")
                .variant(ButtonVariant::Danger)
                .control_size(ControlSize::Small)
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
}
