//! First-class Activity panel (unified Queue + Session Tree + capability-gated
//! Saved approvals & Background shells).
//!
//! Can be rendered either:
//! - Inline inside a session window / split tile (`inline = true`, bound strictly
//!   to that tile's `asid`, zero cross-tile conflicts!), or
//! - Standalone in the Dock / Tile / Page (`inline = false`, with Target Session
//!   Bar and multi-tile Queue overview).

use muqun_gateway::agents::{AgentSessionId, AgentSessionStatus};
use rune::{col, el, row, text, El, ScrollSpec};
use rune_kit::{
    Badge, BadgeVariant, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, ScrollBar,
};

use super::copy::{tr, tr_ref};
use super::git_panel::{section_card, small_btn, sub_card, target_session_bar};
use super::{icons, parts, timeline, UiState};
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, InboxItem};

#[derive(Debug, Default, Clone)]
pub struct ActivityPanelState {
    pub saved_open: bool,
    pub shells_open: bool,
    pub confirm: Option<String>,
}

fn confirm_btn(
    s: &UiState,
    k: &Kit,
    confirm_id: String,
    labels: (&str, &str),
    idle_variant: ButtonVariant,
    key: String,
    on_confirm: impl Fn(&mut UiState) + 'static,
) -> El {
    let armed = s.activity_panel.confirm.as_deref() == Some(confirm_id.as_str());
    let label = if armed { labels.1 } else { labels.0 };
    let variant = if armed {
        ButtonVariant::Danger
    } else {
        idle_variant
    };
    small_btn(k, label, variant, key, move |s| {
        if s.activity_panel.confirm.as_deref() == Some(confirm_id.as_str()) {
            s.activity_panel.confirm = None;
            on_confirm(s);
        } else {
            s.activity_panel.confirm = Some(confirm_id.clone());
        }
    })
}

/// Whether `asid`'s agent supports saved "always allow" permissions
/// (declared via `features.extra["savedPermissions"]`, currently OpenCode).
pub(crate) fn agent_supports_saved_permissions_for(s: &UiState, asid: &str) -> bool {
    if !s.saved_permissions.is_empty() && s.selected.as_deref() == Some(asid) {
        return true;
    }
    let Some(info) = s.session_by_id(asid) else {
        return false;
    };
    s.agents.iter().any(|agent| {
        agent.id == info.agent_id
            && agent
                .features
                .extra
                .get("savedPermissions")
                .and_then(|v| v.as_bool())
                == Some(true)
    })
}

/// Whether `asid`'s agent supports background shells or can park its turn.
pub(crate) fn agent_supports_shells_for(s: &UiState, asid: &str) -> bool {
    if (!s.shells.is_empty() && s.selected.as_deref() == Some(asid))
        || can_background_turn_for(s, asid)
    {
        return true;
    }
    let Some(info) = s.session_by_id(asid) else {
        return false;
    };
    s.agents
        .iter()
        .any(|agent| agent.id == info.agent_id && agent.features.background_shells)
}

pub(crate) fn can_background_turn_for(s: &UiState, asid: &str) -> bool {
    let Some(info) = s.session_by_id(asid) else {
        return false;
    };
    if info.status != AgentSessionStatus::Busy {
        return false;
    }
    s.agents
        .iter()
        .any(|agent| agent.id == info.agent_id && agent.features.background_shells)
}

/// Badge count (waiting approvals/forms + queued prompts) for `asid`.
pub(crate) fn badge_count_for(s: &UiState, asid: &str) -> usize {
    if asid.is_empty() {
        return 0;
    }
    s.waiting_perms.get(asid).map_or(0, Vec::len)
        + s.waiting_forms.get(asid).map_or(0, Vec::len)
        + s.queue_for_session(asid).len()
}

fn render_queue_item(s: &UiState, k: &Kit, asid: &str, q: &InboxItem) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let (id, a) = (q.id.clone(), AgentSessionId(asid.to_string()));
    let steer = q.steer;
    let (id2, a2) = (id.clone(), a.clone());
    let (edit_asid, edit_id, edit_text) = (asid.to_string(), q.id.clone(), q.text.clone());

    let image_files: Vec<&crate::model::compose::QueuedFile> =
        q.files.iter().filter(|f| f.image).collect();
    let mut status_row = row().w_full().gap(6.0).center_y().child(
        Badge::new(if q.steer {
            t("Sending next")
        } else {
            t("Queued")
        })
        .variant(if q.steer {
            BadgeVariant::Accent
        } else {
            BadgeVariant::Subtle
        })
        .build(k),
    );
    if !q.files.is_empty() {
        let att_label = if !image_files.is_empty() {
            format!("{} {}", image_files.len(), t("Image"))
        } else {
            format!("{} {}", q.files.len(), t("Files"))
        };
        status_row = status_row.child(Badge::new(att_label).variant(BadgeVariant::Subtle).build(k));
    }

    let actions = row()
        .w_full()
        .gap(6.0)
        .center_y()
        .child(small_btn(
            k,
            if steer {
                t("Keep queued")
            } else {
                t("Send now")
            },
            if steer {
                ButtonVariant::Outline
            } else {
                ButtonVariant::Neutral
            },
            format!("dl-{asid}-{id}"),
            move |s| {
                s.toggle_queue_steer_for(&a.0, &id, !steer);
                s.send(Command::Compose(ComposeCommand::InboxDelivery {
                    asid: a.clone(),
                    item: id.clone(),
                    steer: !steer,
                }));
            },
        ))
        .child(small_btn(
            k,
            t("Edit"),
            ButtonVariant::Outline,
            format!("ed-{asid}-{edit_id}"),
            move |s| {
                if s.selected.as_deref() != Some(edit_asid.as_str()) {
                    if let Some(tile_idx) = s.tile_of(&edit_asid) {
                        s.focus_tile(tile_idx);
                    }
                }
                super::composer::edit_queued(s, &edit_id, edit_text.clone());
            },
        ))
        .child(el().grow(1.0))
        .child(small_btn(
            k,
            t("Cancel"),
            ButtonVariant::GhostMuted,
            format!("cx-{asid}-{id2}"),
            move |s| {
                s.remove_queue_item_for(&a2.0, &id2);
                s.send(Command::Compose(ComposeCommand::InboxCancel {
                    asid: a2.clone(),
                    item: id2.clone(),
                }));
            },
        ));

    let mut card = sub_card(k, format!("q-{asid}-{}", q.id)).child(status_row);
    if !image_files.is_empty() {
        if let Some(thumb_row) =
            super::composer::queued_file_thumbs(s, k, &format!("act-{asid}-{}", q.id), &image_files)
        {
            card = card.child(thumb_row);
        }
    }
    card.child(
        text(timeline::clip(&q.text, 160))
            .font(k.ty.ui)
            .color(k.c.text_base),
    )
    .child(actions)
}

fn waiting_and_queue_cards(s: &UiState, k: &Kit, target_asid: &str, inline: bool) -> Vec<El> {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let mut out = Vec::new();

    // ---- 1. Waiting on you ----
    let perms = s
        .waiting_perms
        .get(target_asid)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let forms = s
        .waiting_forms
        .get(target_asid)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut waiting_body: Vec<El> = Vec::new();
    waiting_body.extend(
        perms
            .iter()
            .map(|p| timeline::permission_card(s, k, p).key(format!("wp-{target_asid}-{}", p.id))),
    );
    waiting_body.extend(
        forms
            .iter()
            .map(|f| timeline::form_card(s, k, f).key(format!("wf-{target_asid}-{}", f.id))),
    );
    if !inline {
        for (tile_idx, other_asid, other_title) in s.open_tile_sessions() {
            if other_asid == target_asid {
                continue;
            }
            let op = s
                .waiting_perms
                .get(&other_asid)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let of = s
                .waiting_forms
                .get(&other_asid)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if op.is_empty() && of.is_empty() {
                continue;
            }
            let count = op.len() + of.len();
            waiting_body.push(
                sub_card(k, format!("wp-other-{other_asid}")).child(
                    row()
                        .w_full()
                        .gap(6.0)
                        .center_y()
                        .child(
                            Badge::new(format!("Tile {}", tile_idx + 1))
                                .variant(BadgeVariant::Accent)
                                .build(k),
                        )
                        .child(
                            text(format!("{} ({count})", timeline::clip(&other_title, 26)))
                                .font(k.ty.small_medium)
                                .color(k.c.text_base)
                                .truncate()
                                .grow(1.0)
                                .shrink()
                                .min_w(0.0),
                        )
                        .child(small_btn(
                            k,
                            t("Focus"),
                            ButtonVariant::Outline,
                            format!("wp-jump-{other_asid}"),
                            move |s| s.focus_tile(tile_idx),
                        )),
                ),
            );
        }
    }

    if !waiting_body.is_empty() {
        out.push(section_card(
            k,
            format!("activity-waiting-card-{target_asid}"),
            &icons::INBOX,
            t("Waiting on you"),
            Some(waiting_body.len().to_string()),
            Vec::new(),
            waiting_body,
        ));
    }

    // ---- 2. Queued Prompts ----
    let target_queue = s.queue_for_session(target_asid);
    let other_queues: Vec<(usize, String, String, Vec<InboxItem>)> = if inline {
        Vec::new()
    } else {
        s.open_tile_sessions()
            .into_iter()
            .filter(|(_, asid, _)| asid != target_asid)
            .filter_map(|(tile_idx, asid, title)| {
                let items = s.queue_for_session(&asid).to_vec();
                (!items.is_empty()).then_some((tile_idx, asid, title, items))
            })
            .collect()
    };

    let total_queued = target_queue.len()
        + other_queues
            .iter()
            .map(|(_, _, _, items)| items.len())
            .sum::<usize>();

    let mut queue_body = Vec::new();
    if target_queue.is_empty() && other_queues.is_empty() {
        queue_body.push(
            text(t("Messages sent while the agent works line up here."))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    } else {
        for q in target_queue {
            queue_body.push(render_queue_item(s, k, target_asid, q));
        }
        for (tile_idx, other_asid, other_title, items) in other_queues {
            queue_body.push(
                row()
                    .key(format!("other-q-hdr-{other_asid}"))
                    .w_full()
                    .pt(4.0)
                    .gap(6.0)
                    .center_y()
                    .child(
                        Badge::new(format!("Tile {}", tile_idx + 1))
                            .variant(BadgeVariant::Subtle)
                            .build(k),
                    )
                    .child(
                        text(timeline::clip(&other_title, 26))
                            .font(k.ty.small_medium)
                            .color(k.c.text_muted)
                            .truncate()
                            .grow(1.0)
                            .shrink()
                            .min_w(0.0),
                    )
                    .child(small_btn(
                        k,
                        t("Focus"),
                        ButtonVariant::GhostMuted,
                        format!("other-q-jump-{other_asid}"),
                        move |s| s.focus_tile(tile_idx),
                    )),
            );
            for q in &items {
                queue_body.push(render_queue_item(s, k, &other_asid, q));
            }
        }
    }

    out.push(section_card(
        k,
        format!("activity-queue-card-{target_asid}"),
        &parts::glyphs::TIME,
        t("Queue"),
        (total_queued > 0).then(|| total_queued.to_string()),
        Vec::new(),
        queue_body,
    ));

    out
}

fn tree_card(s: &UiState, k: &Kit, asid: &str) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let asid_refresh = asid.to_string();
    let refresh_btn = IconButton::new(&icons::REFRESH)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label(t("Refresh"))
        .help(t("Refresh"))
        .on_press(move |s: &mut UiState| s.refresh_session_tree_for(&asid_refresh))
        .build(k)
        .key(format!("activity-tree-refresh-{asid}"));
    let descendants = s.descendants_for_session(asid);
    let tree_items = super::session_tree::view_for_session(s, k, asid);
    section_card(
        k,
        format!("activity-tree-card-{asid}"),
        &icons::FORK,
        t("Session Tree"),
        (!descendants.is_empty()).then(|| descendants.len().to_string()),
        vec![refresh_btn],
        tree_items,
    )
}

fn saved_card(s: &UiState, k: &Kit, asid: &str) -> Option<El> {
    if !agent_supports_saved_permissions_for(s, asid) {
        return None;
    }
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let open = s.activity_panel.saved_open || !s.saved_permissions.is_empty();
    let asid_load = asid.to_string();

    let mut trailing = vec![small_btn(
        k,
        if open { t("Hide") } else { t("Show") },
        ButtonVariant::GhostMuted,
        format!("activity-saved-toggle-{asid}"),
        move |s| {
            let next = !s.activity_panel.saved_open;
            s.activity_panel.saved_open = next;
            if next && !asid_load.is_empty() {
                s.send(Command::LoadSavedPermissions {
                    asid: AgentSessionId(asid_load.clone()),
                });
            }
        },
    )];
    if open && !asid.is_empty() {
        let a = asid.to_string();
        trailing.push(
            IconButton::new(&icons::REFRESH)
                .variant(IconButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .label(t("Refresh"))
                .help(t("Refresh"))
                .on_press(move |s: &mut UiState| {
                    s.send(Command::LoadSavedPermissions {
                        asid: AgentSessionId(a.clone()),
                    });
                })
                .build(k)
                .key(format!("activity-saved-refresh-{asid}")),
        );
    }

    let mut body = Vec::new();
    if open {
        if s.saved_permissions.is_empty() {
            body.push(
                text(t("No saved permissions for this project."))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            );
        } else {
            for p in &s.saved_permissions {
                let (a, id) = (AgentSessionId(asid.to_string()), p.id.clone());
                let kind = if p.action.is_empty() {
                    tr(language, "rule")
                } else {
                    p.action.clone()
                };
                let resource = if p.resource.is_empty() {
                    p.id.clone()
                } else {
                    p.resource.clone()
                };
                body.push(
                    sub_card(k, format!("sp-{asid}-{}", p.id))
                        .child(
                            row()
                                .w_full()
                                .gap(6.0)
                                .center_y()
                                .child(Badge::new(kind).variant(BadgeVariant::Subtle).build(k))
                                .child(el().grow(1.0))
                                .child(small_btn(
                                    k,
                                    t("Forget"),
                                    ButtonVariant::GhostMuted,
                                    format!("fp-{asid}-{}", p.id),
                                    move |s| {
                                        s.saved_permissions.retain(|x| x.id != id);
                                        s.send(Command::ForgetSavedPermission {
                                            asid: a.clone(),
                                            id: id.clone(),
                                        });
                                    },
                                )),
                        )
                        .child(
                            text(resource)
                                .font(k.ty.mono)
                                .color(k.c.text_base)
                                .truncate(),
                        ),
                );
            }
        }
    }

    Some(section_card(
        k,
        format!("activity-saved-card-{asid}"),
        &parts::glyphs::COMPLETED,
        t("Saved approvals"),
        (!s.saved_permissions.is_empty()).then(|| s.saved_permissions.len().to_string()),
        trailing,
        body,
    ))
}

fn shells_card(s: &UiState, k: &Kit, asid: &str) -> Option<El> {
    if !agent_supports_shells_for(s, asid) {
        return None;
    }
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let open =
        s.activity_panel.shells_open || !s.shells.is_empty() || can_background_turn_for(s, asid);
    let (agent, dir) = s
        .session_by_id(asid)
        .map(|i| (i.agent_id.clone(), i.directory.clone()))
        .unwrap_or_default();
    let (a_toggle, d_toggle) = (agent.clone(), dir.clone());
    let mut trailing = vec![small_btn(
        k,
        if open { t("Hide") } else { t("Show") },
        ButtonVariant::GhostMuted,
        format!("activity-shells-toggle-{asid}"),
        move |s| {
            let next = !s.activity_panel.shells_open;
            s.activity_panel.shells_open = next;
            if next && !a_toggle.is_empty() {
                s.send(Command::LoadShells {
                    agent: a_toggle.clone(),
                    directory: d_toggle.clone(),
                });
            }
        },
    )];
    if open && !agent.is_empty() {
        let (a_ref, d_ref) = (agent.clone(), dir.clone());
        trailing.push(
            IconButton::new(&icons::REFRESH)
                .variant(IconButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .label(t("Refresh"))
                .help(t("Refresh"))
                .on_press(move |s: &mut UiState| {
                    s.send(Command::LoadShells {
                        agent: a_ref.clone(),
                        directory: d_ref.clone(),
                    });
                })
                .build(k)
                .key(format!("activity-shells-refresh-{asid}")),
        );
    }

    let mut body = Vec::new();
    if open {
        if can_background_turn_for(s, asid) && !asid.is_empty() {
            let bg_asid = asid.to_string();
            body.push(small_btn(
                k,
                t("Move turn to background"),
                ButtonVariant::Outline,
                format!("bg-session-{asid}"),
                move |s| {
                    s.send(Command::BackgroundSession {
                        asid: AgentSessionId(bg_asid.clone()),
                    });
                },
            ));
        }
        if s.shells.is_empty() {
            body.push(
                text(t("No background shells running."))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            );
        } else {
            for sh in &s.shells {
                let (a, id, d) = (agent.clone(), sh.id.clone(), dir.clone());
                let confirm_key = format!("stop-sh:{asid}:{}", sh.id);
                let stop_btn = confirm_btn(
                    s,
                    k,
                    confirm_key,
                    (t("Stop"), t("Confirm")),
                    ButtonVariant::GhostMuted,
                    format!("ks-{asid}-{}", sh.id),
                    move |s| {
                        s.shells.retain(|x| x.id != id);
                        s.send(Command::KillShell {
                            agent: a.clone(),
                            id: id.clone(),
                            directory: d.clone(),
                        });
                    },
                );
                let title = if sh.command.is_empty() {
                    sh.id.clone()
                } else {
                    sh.command.clone()
                };
                body.push(
                    sub_card(k, format!("sh-{asid}-{}", sh.id))
                        .child(
                            row()
                                .w_full()
                                .gap(6.0)
                                .center_y()
                                .child(
                                    Badge::new(if sh.status.is_empty() {
                                        tr(language, "running")
                                    } else {
                                        sh.status.clone()
                                    })
                                    .variant(BadgeVariant::Subtle)
                                    .build(k),
                                )
                                .child(el().grow(1.0))
                                .child(stop_btn),
                        )
                        .child(text(title).font(k.ty.mono).color(k.c.text_base).truncate()),
                );
            }
        }
    }

    Some(section_card(
        k,
        format!("activity-shells-card-{asid}"),
        &parts::glyphs::TERMINAL,
        t("Background shells"),
        (!s.shells.is_empty()).then(|| s.shells.len().to_string()),
        trailing,
        body,
    ))
}

/// Render the Activity panel for a specific session `asid`.
/// When `inline == true`, renders without the Target Session Bar (used inside a tile drawer).
pub(crate) fn view_for_session(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    asid: &str,
    inline: bool,
) -> El {
    let mut cards = waiting_and_queue_cards(s, k, asid, inline);
    cards.push(tree_card(s, k, asid));
    if let Some(saved) = saved_card(s, k, asid) {
        cards.push(saved);
    }
    if let Some(shells) = shells_card(s, k, asid) {
        cards.push(shells);
    }

    let scroll = col()
        .key(format!("activity-panel-scroll-{asid}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(12.0)
        .py(12.0)
        .gap(10.0)
        .children(cards)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_bar(k);

    let mut root = col()
        .key(if inline {
            format!("activity-panel-inline-{asid}")
        } else {
            "activity-panel".to_string()
        })
        .w_full()
        .h_full()
        .min_h(0.0);
    if !inline {
        root = root.child(target_session_bar(s, k, layer, "activity"));
    }
    root.child(scroll)
}

/// Render the standalone top-level Activity plugin panel.
pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let asid = s.panel_target_asid().unwrap_or_default().to_string();
    view_for_session(s, k, layer, &asid, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::AgentSessionInfo;
    use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
    use serde_json::json;

    fn fake_session(asid: &str, agent_id: &str) -> AgentSessionInfo {
        serde_json::from_value(json!({
            "asid": asid,
            "agent_id": agent_id,
            "title": format!("Session {asid}"),
            "directory": "/tmp/repo",
            "status": "idle",
            "updated_ms": 100
        }))
        .expect("session fixture")
    }

    fn fake_agent(
        id: &str,
        background_shells: bool,
        saved_permissions: bool,
    ) -> AgentDiscoveryInfo {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "kind": id,
            "status": "connected",
            "enabled": true,
            "models": [],
            "modes": [],
            "features": {
                "streaming": true,
                "reasoningEffort": false,
                "modelSelection": true,
                "toolApprovals": true,
                "worktrees": false,
                "revert": false,
                "stagedRevert": false,
                "inbox": true,
                "modes": false,
                "skills": false,
                "slashCommands": false,
                "compaction": false,
                "backgroundShells": background_shells,
                "attachments": true,
                "attachmentsByPath": false,
                "savedPermissions": saved_permissions
            }
        }))
        .expect("agent fixture")
    }

    #[test]
    fn saved_and_shells_are_hidden_for_unsupported_agents_and_shown_for_opencode() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.agents.push(fake_agent("opencode", true, true));
        s.agents.push(fake_agent("codex", false, false));
        s.sessions.push(fake_session("s-codex", "codex"));
        s.sessions.push(fake_session("s-oc", "opencode"));

        assert!(!agent_supports_saved_permissions_for(&s, "s-codex"));
        assert!(!agent_supports_shells_for(&s, "s-codex"));

        assert!(agent_supports_saved_permissions_for(&s, "s-oc"));
        assert!(agent_supports_shells_for(&s, "s-oc"));
    }
}
