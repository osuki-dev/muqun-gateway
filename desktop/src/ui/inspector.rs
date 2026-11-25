//! The right-hand inspector: Queue, Git (with worktrees), Tree, Saved, Shells.

use std::path::PathBuf;

use muqun_gateway::agents::AgentSessionId;
use rune::{col, el, row, text, Color, El, Icon, ScrollSpec, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit,
    ScrollBar, TextInput,
};

use super::copy::{tr, tr_ref};
use super::{icons, parts, timeline, InspectorTab, Mode, OpenMenu, UiState};
use crate::config::Language;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, GitRemoteHost};

const TABS: [(&str, InspectorTab); 5] = [
    ("Queue", InspectorTab::Queue),
    ("Git", InspectorTab::Git),
    ("Tree", InspectorTab::Subagents),
    ("Saved", InspectorTab::Saved),
    ("Shells", InspectorTab::Shells),
];

/// Initial number of changed files shown before the explicit "Show all" toggle.
const INITIAL_DIFF_FILES: usize = 12;

/// The tab strip's height: matches the session toolbar so the two align.
const TOOLBAR_H: f32 = rune_kit::shell::TOOLBAR_HEIGHT;

fn small_btn(
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

/// Two-step confirmation button for destructive actions (discard, remove, stop).
fn confirm_btn(
    s: &UiState,
    k: &Kit,
    confirm_id: String,
    labels: (&str, &str),
    idle_variant: ButtonVariant,
    key: String,
    on_confirm: impl Fn(&mut UiState) + 'static,
) -> El {
    let armed = s.inspector_confirm.as_deref() == Some(confirm_id.as_str());
    let label = if armed { labels.1 } else { labels.0 };
    let variant = if armed {
        ButtonVariant::Danger
    } else {
        idle_variant
    };
    small_btn(k, label, variant, key, move |s| {
        if s.inspector_confirm.as_deref() == Some(confirm_id.as_str()) {
            s.inspector_confirm = None;
            on_confirm(s);
        } else {
            s.inspector_confirm = Some(confirm_id.clone());
        }
    })
}

fn tab_label(language: Language, en: &'static str) -> &'static str {
    match en {
        "Queue" => tr_ref(language, "Queue"),
        "Git" => "Git",
        "Tree" => tr_ref(language, "Tree"),
        "Saved" => tr_ref(language, "Saved"),
        "Shells" => tr_ref(language, "Shells"),
        other => other,
    }
}

/// Count badge for each inspector tab; `None` when zero.
pub(crate) fn tab_count(s: &UiState, tab: InspectorTab) -> Option<usize> {
    let asid = s.selected.as_deref().unwrap_or("");
    let n = match tab {
        InspectorTab::Queue => {
            s.waiting_perms.get(asid).map_or(0, Vec::len)
                + s.waiting_forms.get(asid).map_or(0, Vec::len)
                + s.queue.len()
        }
        InspectorTab::Git => s.diff_stat.files,
        InspectorTab::Subagents => s.descendants.len(),
        InspectorTab::Saved => s.saved_permissions.len(),
        InspectorTab::Shells => s.shells.len(),
    };
    (n > 0).then_some(n)
}

/// One compact segmented pill tab with an optional count badge.
fn tab_button(k: &Kit, label: &str, count: Option<usize>, tab: InspectorTab, on: bool) -> El {
    let ink = if on { k.c.text_base } else { k.c.text_muted };
    let mut b = row()
        .key(format!("tab-{tab:?}"))
        .h(26.0)
        .px(8.0)
        .gap(5.0)
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
                .font(if on { k.ty.small_medium } else { k.ty.small })
                .color(ink)
                .truncate()
                .shrink()
                .a11y_hidden(),
        );
    if let Some(n) = count {
        let accent = on || tab == InspectorTab::Queue;
        b = b.child(
            row()
                .h(16.0)
                .px(5.0)
                .center()
                .radius(k.radius.sm)
                .fill(if accent {
                    k.c.bg_accent.alpha(0.18)
                } else {
                    k.c.layer_01
                })
                .child(
                    text(n.to_string())
                        .font(k.ty.small_medium)
                        .color(if accent {
                            k.c.text_accent
                        } else {
                            k.c.text_muted
                        })
                        .a11y_hidden(),
                ),
        );
    }
    if on {
        b = b.fill(k.c.surface_raised_base).border(1.0, k.c.border_weak);
    }
    k.interactive(b).on_press(move |s: &mut UiState| {
        s.inspector_tab = tab;
        s.inspector_confirm = None;
        if s.menu == Some(OpenMenu::InspectorBranch) {
            s.menu = None;
        }
        s.refresh_inspector();
    })
}

/// Elevated inspector section card with an icon, title, optional count badge,
/// optional trailing action controls, and structured body.
fn section_card(
    k: &Kit,
    key: impl Into<String>,
    glyph: &'static Icon,
    title: impl Into<String>,
    badge: Option<String>,
    trailing: Vec<El>,
    body: Vec<El>,
) -> El {
    let mut header = row()
        .w_full()
        .center_y()
        .gap(6.0)
        .px(12.0)
        .py(9.0)
        .child(rune::icon(glyph, 14.0).color(k.c.icon_muted).a11y_hidden())
        .child(
            text(title.into())
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate()
                .shrink()
                .min_w(0.0),
        );
    if let Some(badge) = badge {
        header = header.child(Badge::new(badge).variant(BadgeVariant::Subtle).build(k));
    }
    header = header.child(el().grow(1.0));
    if !trailing.is_empty() {
        header = header.child(row().gap(4.0).center_y().children(trailing));
    }

    let mut card = col()
        .key(key.into())
        .w_full()
        .radius(k.radius.lg)
        .fill(k.c.surface_raised_base)
        .border(1.0, k.c.border_weak)
        .child(header);
    if !body.is_empty() {
        card = card
            .child(el().w_full().h(1.0).fill(k.c.border_weak).a11y_hidden())
            .child(col().w_full().gap(8.0).px(12.0).py(10.0).children(body));
    }
    card
}

/// Nested row container inside a section card.
fn sub_card(k: &Kit, key: String) -> El {
    col()
        .key(key)
        .w_full()
        .gap(6.0)
        .px(10.0)
        .py(8.0)
        .radius(k.radius.md)
        .fill(k.c.layer_01.alpha(0.55))
        .border(1.0, k.c.border_weak.alpha(0.75))
}

/// Polished empty state rendered inside a card (or standalone when a whole tab is empty).
fn empty_notice(
    k: &Kit,
    key: &str,
    glyph: Option<&'static Icon>,
    title: &str,
    detail: &str,
    action: Option<El>,
) -> El {
    let mut head = row().w_full().center_y().gap(6.0);
    if let Some(icon) = glyph {
        head = head.child(rune::icon(icon, 14.0).color(k.c.icon_muted).a11y_hidden());
    }
    head = head.child(
        text(title.to_string())
            .font(k.ty.ui_medium)
            .color(k.c.text_muted),
    );
    let mut box_el = col()
        .key(format!("empty-{key}"))
        .w_full()
        .gap(6.0)
        .px(12.0)
        .py(12.0)
        .radius(k.radius.lg)
        .fill(k.c.layer_01.alpha(0.45))
        .border(1.0, k.c.border_weak)
        .child(head)
        .child(
            text(detail.to_string())
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    if let Some(act) = action {
        box_el = box_el.child(row().pt(2.0).child(act));
    }
    box_el
}

/// Shorten a filesystem path for compact display inside narrow cards.
fn short_path(path: &str) -> String {
    let trimmed = path.trim();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && trimmed.starts_with(&home) {
            let rest = trimmed[home.len()..].trim_start_matches('/');
            if rest.is_empty() {
                return "~".to_string();
            }
            return format!("~/{rest}");
        }
    }
    trimmed.to_string()
}

fn host_display_path(web_url: &str) -> &str {
    web_url
        .strip_prefix("https://")
        .or_else(|| web_url.strip_prefix("http://"))
        .unwrap_or(web_url)
}

fn queue(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
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
    let waiting_total = perms.len() + forms.len();

    if waiting_total > 0 {
        let mut waiting_body = Vec::with_capacity(waiting_total);
        waiting_body.extend(
            perms
                .iter()
                .map(|p| timeline::permission_card(s, k, p).key(format!("wp-{}", p.id))),
        );
        waiting_body.extend(
            forms
                .iter()
                .map(|f| timeline::form_card(s, k, f).key(format!("wf-{}", f.id))),
        );
        out.push(section_card(
            k,
            "queue-waiting-card",
            &icons::INBOX,
            t("Waiting on you"),
            Some(waiting_total.to_string()),
            Vec::new(),
            waiting_body,
        ));
    }

    if s.queue.is_empty() {
        if waiting_total == 0 {
            out.push(empty_notice(
                k,
                "queue",
                Some(&parts::glyphs::TIME),
                t("Nothing queued"),
                t("Messages sent while the agent works line up here."),
                None,
            ));
        } else {
            out.push(section_card(
                k,
                "queue-prompts-card",
                &parts::glyphs::TIME,
                t("Queued prompts"),
                Some("0".to_string()),
                Vec::new(),
                vec![text(t("Messages sent while the agent works line up here."))
                    .font(k.ty.small)
                    .color(k.c.text_faint)],
            ));
        }
        return out;
    }

    let mut prompt_cards = Vec::with_capacity(s.queue.len());
    for q in &s.queue {
        let (id, a) = (q.id.clone(), AgentSessionId(asid.clone()));
        let steer = q.steer;
        let (id2, a2) = (id.clone(), a.clone());
        let (edit_id, edit_text) = (q.id.clone(), q.text.clone());

        let images = q.files.iter().filter(|f| f.image).count();
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
            let att_label = if images > 0 {
                format!("{images} {}", t("Image"))
            } else {
                format!("{} {}", q.files.len(), t("Files"))
            };
            status_row =
                status_row.child(Badge::new(att_label).variant(BadgeVariant::Subtle).build(k));
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
            .child(small_btn(
                k,
                t("Edit"),
                ButtonVariant::Outline,
                format!("ed-{edit_id}"),
                move |s| {
                    super::composer::edit_queued(s, &edit_id, edit_text.clone());
                },
            ))
            .child(el().grow(1.0))
            .child(small_btn(
                k,
                t("Cancel"),
                ButtonVariant::GhostMuted,
                format!("cx-{id2}"),
                move |s| {
                    s.queue.retain(|x| x.id != id2);
                    s.send(Command::Compose(ComposeCommand::InboxCancel {
                        asid: a2.clone(),
                        item: id2.clone(),
                    }));
                },
            ));

        let mut card = sub_card(k, format!("q-{}", q.id)).child(status_row);
        let thumbs: Vec<&str> = q
            .files
            .iter()
            .filter(|f| f.image)
            .map(|f| f.path.as_str())
            .collect();
        if let Some(thumb_row) = super::composer::queue_thumbs(s, k, &q.id, &thumbs) {
            card = card.child(thumb_row);
        }
        card = card
            .child(
                text(timeline::clip(&q.text, 140))
                    .font(k.ty.ui)
                    .color(k.c.text_base),
            )
            .child(actions);
        prompt_cards.push(card);
    }

    out.push(section_card(
        k,
        "queue-prompts-card",
        &parts::glyphs::TIME,
        t("Queued prompts"),
        Some(s.queue.len().to_string()),
        Vec::new(),
        prompt_cards,
    ));
    out
}

/// Whether local git mutations (branch switch, discard, commit, push) are
/// permitted for the current session: must be a local session inside a git repo.
pub(crate) fn can_run_local_git(s: &UiState) -> bool {
    if s.is_remote_session() {
        return false;
    }
    let Some(cwd) = s.current().and_then(|i| i.directory.as_deref()) else {
        return false;
    };
    s.branches.get(cwd).is_some_and(|b| b.current.is_some())
}

/// Whether the Git tab should show the clean-tree "Push commits" button.
pub(crate) fn should_show_push_commits(s: &UiState) -> bool {
    s.diff_stat.files == 0 && can_run_local_git(s)
}

/// Number of changed file rows currently displayed in the Changes card.
pub(crate) fn visible_diff_file_count(s: &UiState) -> usize {
    let total = s.diff_stat.paths.len();
    if s.inspector_git_show_all {
        total
    } else {
        INITIAL_DIFF_FILES.min(total)
    }
}

fn branch_switcher_dropdown(
    s: &UiState,
    k: &Kit,
    cwd: &str,
    cur_branch: &str,
    local: &[String],
) -> El {
    let language = s.cfg.general.language;
    let open = s.menu == Some(OpenMenu::InspectorBranch);
    let can_switch = !local.is_empty();
    let chevron = if open { "▴" } else { "▾" };

    let mut trigger = row()
        .key("git-branch-switcher-trigger")
        .w_full()
        .h(32.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.md)
        .fill(k.c.layer_01)
        .border(1.0, if open { k.c.bg_accent } else { k.c.border_weak })
        .role("button")
        .label(tr_ref(language, "Switch branch"))
        .focusable(can_switch)
        .child(
            rune::icon(&icons::BRANCH, 14.0)
                .color(k.c.text_accent)
                .a11y_hidden(),
        )
        .child(
            text(cur_branch.to_string())
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate()
                .grow(1.0)
                .shrink()
                .min_w(0.0),
        )
        .child(
            text(chevron)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .a11y_hidden(),
        );
    if can_switch {
        trigger = k.interactive(trigger).on_press(move |s: &mut UiState| {
            s.menu = if s.menu == Some(OpenMenu::InspectorBranch) {
                None
            } else {
                Some(OpenMenu::InspectorBranch)
            };
        });
    }

    let mut wrap = col()
        .key("git-branch-switcher")
        .w_full()
        .gap(4.0)
        .child(trigger);

    if open && can_switch {
        let mut list = col()
            .key("git-branch-list")
            .w_full()
            .gap(2.0)
            .px(4.0)
            .py(4.0)
            .radius(k.radius.md)
            .fill(k.c.layer_01)
            .border(1.0, k.c.border_weak);
        for branch in local {
            let is_current = branch == cur_branch;
            let (cwd_owned, target_branch) = (cwd.to_string(), branch.clone());
            let mut item = row()
                .key(format!("git-branch-opt-{branch}"))
                .w_full()
                .h(28.0)
                .px(8.0)
                .gap(6.0)
                .center_y()
                .radius(k.radius.sm)
                .child(
                    text(if is_current { "✓" } else { " " })
                        .font(k.ty.small_medium)
                        .color(k.c.text_accent)
                        .a11y_hidden(),
                )
                .child(
                    text(branch.clone())
                        .font(if is_current {
                            k.ty.small_medium
                        } else {
                            k.ty.small
                        })
                        .color(if is_current {
                            k.c.text_base
                        } else {
                            k.c.text_muted
                        })
                        .truncate()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0),
                );
            if is_current {
                item = item.fill(k.c.bg_accent.alpha(0.14));
            }
            item = k.interactive(item).on_press(move |s: &mut UiState| {
                s.menu = None;
                if !is_current {
                    s.send(Command::Compose(ComposeCommand::Checkout {
                        cwd: cwd_owned.clone(),
                        branch: target_branch.clone(),
                    }));
                }
            });
            list = list.child(item);
        }
        wrap = wrap.child(list);
    }

    wrap
}

fn git(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let mut out = Vec::new();
    let info = s.current();
    let cwd = info.and_then(|i| i.directory.clone());
    let is_remote = s.is_remote_session();
    let branch_info = if is_remote {
        None
    } else {
        cwd.as_ref().and_then(|c| s.branches.get(c))
    };
    let is_git_repo = !is_remote && branch_info.is_some_and(|b| b.current.is_some());

    // ---- 1. Repository / Branch (or Remote Host) Card ----
    let refresh_btn = IconButton::new(&icons::REFRESH)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label(t("Refresh"))
        .help(t("Refresh"))
        .on_press(|s: &mut UiState| s.refresh_inspector())
        .build(k)
        .key("git-refresh-btn");

    if is_remote {
        let host_name = s.computer_name(&s.active_session_computer());
        let cwd_path = cwd.as_ref().map(PathBuf::from);
        let open_term = small_btn(
            k,
            t("Open in Terminal"),
            ButtonVariant::Outline,
            "git-open-term".into(),
            move |s| {
                let dir = cwd_path.clone().unwrap_or_else(|| s.terminal_cwd());
                s.term_new_tab_in(dir, Some("git status\n".into()));
            },
        );
        let mut body = Vec::new();
        if let Some(dir) = &cwd {
            body.push(
                text(short_path(dir))
                    .font(k.ty.mono)
                    .color(k.c.text_muted)
                    .truncate(),
            );
        }
        body.push(
            text(t("Git branch switching, commit, discard, and push run locally. View diffs or open a remote terminal below."))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
        body.push(row().gap(6.0).center_y().child(open_term));
        out.push(section_card(
            k,
            "git-remote-host-card",
            &parts::glyphs::COMPUTER,
            t("Remote host"),
            Some(host_name),
            vec![refresh_btn],
            body,
        ));
    } else if let Some(b) = branch_info.filter(|b| b.current.is_some()) {
        let cur_branch = b.current.clone().unwrap_or_else(|| "detached".into());
        let cwd_str = cwd.clone().unwrap_or_default();
        let cwd_path = cwd.as_ref().map(PathBuf::from);
        let open_term = small_btn(
            k,
            t("Terminal"),
            ButtonVariant::Outline,
            "git-open-term".into(),
            move |s| {
                let dir = cwd_path.clone().unwrap_or_else(|| s.terminal_cwd());
                s.term_new_tab_in(dir, Some("git status\n".into()));
            },
        );
        let local_badge = (!b.local.is_empty()).then(|| {
            format!(
                "{} {}",
                b.local.len(),
                if b.local.len() == 1 {
                    t("local branch")
                } else {
                    t("local branches")
                }
            )
        });

        let mut body = vec![branch_switcher_dropdown(
            s,
            k,
            &cwd_str,
            &cur_branch,
            &b.local,
        )];

        if let Some(repo_url) = b.remote_url.as_deref() {
            let host = b
                .remote_host
                .unwrap_or_else(|| GitRemoteHost::detect(repo_url));
            let is_feature = !matches!(cur_branch.as_str(), "main" | "master" | "detached");
            let pr_url = host.pr_url(repo_url, Some(&cur_branch));
            let repo_view_url = repo_url.to_string();

            let mut link_row = row().w_full().gap(6.0).center_y().flex_wrap();
            if host != GitRemoteHost::Remote {
                let pr_label = t(host.pr_action_label(is_feature));
                link_row = link_row.child(small_btn(
                    k,
                    pr_label,
                    ButtonVariant::Outline,
                    "git-pr-btn".into(),
                    move |_| {
                        crate::platform::shell::open_url(&pr_url);
                    },
                ));
            }
            link_row = link_row.child(small_btn(
                k,
                t(host.view_repo_label()),
                ButtonVariant::Neutral,
                "git-view-repo".into(),
                move |_| {
                    crate::platform::shell::open_url(&repo_view_url);
                },
            ));

            body.push(
                col()
                    .key("git-remote-links")
                    .w_full()
                    .gap(6.0)
                    .pt(2.0)
                    .child(
                        row()
                            .w_full()
                            .gap(6.0)
                            .center_y()
                            .child(
                                rune::icon(&icons::OPEN_EXTERNAL, 13.0)
                                    .color(k.c.icon_muted)
                                    .a11y_hidden(),
                            )
                            .child(
                                text(host_display_path(repo_url).to_string())
                                    .font(k.ty.small)
                                    .color(k.c.text_muted)
                                    .truncate()
                                    .shrink()
                                    .min_w(0.0),
                            ),
                    )
                    .child(link_row),
            );
        }

        out.push(section_card(
            k,
            "git-branch-card",
            &icons::BRANCH,
            t("Branch"),
            local_badge,
            vec![open_term, refresh_btn],
            body,
        ));
    } else {
        let cwd_path = cwd.as_ref().map(PathBuf::from);
        let init_btn = small_btn(
            k,
            t("Initialize Git in Terminal"),
            ButtonVariant::Outline,
            "git-init-term".into(),
            move |s| {
                let dir = cwd_path.clone().unwrap_or_else(|| s.terminal_cwd());
                s.term_new_tab_in(dir, Some("git init && git status\n".into()));
            },
        );
        out.push(section_card(
            k,
            "git-no-repo-card",
            &icons::BRANCH,
            t("Branch"),
            None,
            vec![refresh_btn],
            vec![
                text(t("No repository"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_muted),
                text(t("This folder is not a git checkout."))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
                row().pt(2.0).child(init_btn),
            ],
        ));
    }

    // ---- 2. Changed Files Card ----
    let view_diff_btn = (s.diff_stat.files > 0).then(|| {
        small_btn(
            k,
            t("View diff"),
            ButtonVariant::Outline,
            "view-changes".into(),
            |s| {
                s.mode = Mode::Session;
                s.show_diff = true;
            },
        )
    });

    if s.diff_stat.files == 0 {
        let mut body = vec![
            text(t("Working tree is clean"))
                .font(k.ty.ui_medium)
                .color(k.c.text_muted),
            text(t("Edits the agent makes show up here."))
                .font(k.ty.small)
                .color(k.c.text_faint),
        ];
        if should_show_push_commits(s) {
            body.push(row().pt(2.0).child(small_btn(
                k,
                t("Push commits"),
                ButtonVariant::Outline,
                "git-push-btn".into(),
                push_changes,
            )));
        }
        out.push(section_card(
            k,
            "git-changes-card",
            &parts::glyphs::DIFF,
            t("Changes"),
            None,
            Vec::new(),
            body,
        ));
    } else {
        let total_paths = s.diff_stat.paths.len();
        let visible_limit = visible_diff_file_count(s);

        let summary = row()
            .key("git-diff-summary")
            .w_full()
            .gap(8.0)
            .center_y()
            .child(
                text(format!(
                    "{} {}",
                    s.diff_stat.files,
                    if s.diff_stat.files == 1 {
                        t("changed file")
                    } else {
                        t("changed files")
                    }
                ))
                .font(k.ty.small)
                .color(k.c.text_muted),
            )
            .child(el().grow(1.0))
            .child(
                text(format!("+{}", s.diff_stat.added))
                    .font(k.ty.small_medium)
                    .color(k.c.success.fg),
            )
            .child(
                text(format!("−{}", s.diff_stat.removed))
                    .font(k.ty.small_medium)
                    .color(k.c.danger.fg),
            );

        let mut body = Vec::with_capacity(visible_limit + 2);
        body.push(summary);

        for (i, f) in s.diff_stat.paths.iter().take(visible_limit).enumerate() {
            let p = f.path.clone();
            let asid = s.selected.clone().unwrap_or_default();
            let diff_btn = small_btn(
                k,
                t("Diff"),
                ButtonVariant::Outline,
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

            let mut actions = row().gap(4.0).center_y().child(diff_btn);
            if is_git_repo {
                let discard_path = f.path.clone();
                let confirm_key = format!("discard:{discard_path}");
                actions = actions.child(confirm_btn(
                    s,
                    k,
                    confirm_key,
                    (t("Discard"), t("Confirm discard")),
                    ButtonVariant::GhostMuted,
                    format!("discard-item-{i}"),
                    move |s| {
                        let cwd = s
                            .current()
                            .and_then(|info| info.directory.clone())
                            .unwrap_or_else(|| s.terminal_cwd().display().to_string());
                        if !cwd.is_empty() {
                            s.send(Command::Compose(ComposeCommand::DiscardFile {
                                cwd,
                                path: discard_path.clone(),
                                asid: s.selected.clone().map(AgentSessionId),
                            }));
                        }
                    },
                ));
            }

            let file_row = row()
                .key(format!("gf-{}", f.path))
                .w_full()
                .gap(8.0)
                .center_y()
                .px(10.0)
                .py(7.0)
                .radius(k.radius.md)
                .fill(k.c.layer_01.alpha(0.55))
                .border(1.0, k.c.border_weak.alpha(0.75))
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(2.0)
                        .child(
                            text(f.path.clone())
                                .font(k.ty.small_medium)
                                .color(k.c.text_base)
                                .truncate(),
                        )
                        .child(
                            row()
                                .gap(6.0)
                                .center_y()
                                .child(
                                    text(format!("+{}", f.added))
                                        .font(k.ty.small)
                                        .color(k.c.success.fg),
                                )
                                .child(
                                    text(format!("−{}", f.removed))
                                        .font(k.ty.small)
                                        .color(k.c.danger.fg),
                                ),
                        ),
                )
                .child(actions);
            body.push(file_row);
        }

        if total_paths > INITIAL_DIFF_FILES {
            let expanded = s.inspector_git_show_all;
            let toggle_label = if expanded {
                t("Show fewer").to_string()
            } else {
                format!("{} {} {}", t("Show all"), total_paths, t("files"))
            };
            body.push(
                small_btn(
                    k,
                    &toggle_label,
                    ButtonVariant::Outline,
                    "git-toggle-all-files".into(),
                    move |s| {
                        s.inspector_git_show_all = !expanded;
                    },
                )
                .w_full(),
            );
        }

        out.push(section_card(
            k,
            "git-changes-card",
            &parts::glyphs::DIFF,
            t("Changes"),
            Some(s.diff_stat.files.to_string()),
            view_diff_btn.into_iter().collect(),
            body,
        ));

        // ---- 3. Commit & Push Card (local git repos only) ----
        if is_git_repo {
            let summary_line = format!(
                "{} {} {}",
                t("Will stage & commit"),
                s.diff_stat.files,
                if s.diff_stat.files == 1 {
                    t("changed file")
                } else {
                    t("changed files")
                }
            );
            let commit_input = TextInput::new(&s.git_commit_msg)
                .placeholder(t("Commit message…"))
                .label(t("Commit message"))
                .on_edit(|s: &mut UiState| &mut s.git_commit_msg)
                .on_submit(|s: &mut UiState, _: &mut Tx| commit_changes(s, false))
                .build(k)
                .key("git-commit-msg-field")
                .w_full();

            let has_msg = !s.git_commit_msg.text.trim().is_empty();
            let commit_btn = Button::new(t("Commit"))
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Small)
                .disabled(!has_msg)
                .on_press(|s: &mut UiState| commit_changes(s, false))
                .build(k)
                .key("git-commit-btn");

            let commit_push_btn = Button::new(t("Commit & Push"))
                .variant(ButtonVariant::Submit)
                .control_size(ControlSize::Small)
                .disabled(!has_msg)
                .on_press(|s: &mut UiState| commit_changes(s, true))
                .build(k)
                .key("git-commit-push-btn");

            out.push(section_card(
                k,
                "git-commit-box",
                &parts::glyphs::COMPLETED,
                t("Commit & Push"),
                None,
                Vec::new(),
                vec![
                    text(summary_line).font(k.ty.small).color(k.c.text_muted),
                    commit_input,
                    row()
                        .w_full()
                        .gap(6.0)
                        .center_y()
                        .child(commit_btn)
                        .child(commit_push_btn),
                ],
            ));
        }
    }

    // ---- 4. Worktrees Card ----
    let (agent, dir) = info
        .map(|i| (i.agent_id.clone(), i.directory.clone().unwrap_or_default()))
        .unwrap_or_default();

    let rediscover_btn = (!dir.is_empty()).then(|| {
        let (agent, directory) = (agent.clone(), dir.clone());
        small_btn(
            k,
            t("Rediscover"),
            ButtonVariant::Outline,
            "wt-refresh".into(),
            move |s| {
                s.send(Command::RefreshWorktrees {
                    agent: agent.clone(),
                    directory: directory.clone(),
                });
            },
        )
    });

    let wt_body = if s.worktrees.is_empty() {
        vec![
            text(t("No linked worktrees"))
                .font(k.ty.ui_medium)
                .color(k.c.text_muted),
            text(t("Start a session in a new worktree from Home."))
                .font(k.ty.small)
                .color(k.c.text_faint),
        ]
    } else {
        s.worktrees
            .iter()
            .map(|w| {
                let dir_path = PathBuf::from(&w.directory);
                let mut actions = row().gap(4.0).center_y().child(small_btn(
                    k,
                    t("Terminal"),
                    ButtonVariant::Outline,
                    format!("term-wt-{}", w.directory),
                    move |s| {
                        s.term_new_tab_in(dir_path.clone(), None);
                    },
                ));
                if !w.is_root() {
                    let (agent, project, worktree) =
                        (agent.clone(), dir.clone(), w.directory.clone());
                    let confirm_key = format!("rm-wt:{}", w.directory);
                    actions = actions.child(confirm_btn(
                        s,
                        k,
                        confirm_key,
                        (t("Remove"), t("Confirm remove")),
                        ButtonVariant::GhostMuted,
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
                let strategy = w
                    .strategy
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| tr(language, "root"));
                sub_card(k, format!("wt-{}", w.directory))
                    .child(
                        row()
                            .w_full()
                            .gap(6.0)
                            .center_y()
                            .child(Badge::new(strategy).variant(BadgeVariant::Subtle).build(k))
                            .child(el().grow(1.0))
                            .child(actions),
                    )
                    .child(
                        text(short_path(&w.directory))
                            .font(k.ty.mono)
                            .color(k.c.text_muted)
                            .truncate(),
                    )
            })
            .collect()
    };

    out.push(section_card(
        k,
        "git-worktrees-card",
        &icons::FORK,
        t("Worktrees"),
        (!s.worktrees.is_empty()).then(|| s.worktrees.len().to_string()),
        rediscover_btn.into_iter().collect(),
        wt_body,
    ));

    out
}

fn saved(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let asid = s.selected.clone().unwrap_or_default();

    let refresh_btn = (!asid.is_empty()).then(|| {
        let a = AgentSessionId(asid.clone());
        small_btn(
            k,
            t("Refresh"),
            ButtonVariant::Outline,
            "saved-refresh".into(),
            move |s| {
                s.send(Command::LoadSavedPermissions { asid: a.clone() });
            },
        )
    });

    if s.saved_permissions.is_empty() {
        return vec![section_card(
            k,
            "saved-card",
            &parts::glyphs::TRUST_WARNING,
            t("Saved permissions"),
            Some("0".to_string()),
            refresh_btn.into_iter().collect(),
            vec![
                text(t("Nothing remembered"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_muted),
                text(t(
                    "\"Always allow\" answers for this project are kept here.",
                ))
                .font(k.ty.small)
                .color(k.c.text_faint),
            ],
        )];
    }

    let rows: Vec<El> = s
        .saved_permissions
        .iter()
        .map(|p| {
            let (id, a) = (p.id.clone(), AgentSessionId(asid.clone()));
            let forget = small_btn(
                k,
                t("Forget"),
                ButtonVariant::Outline,
                format!("fg-{}", p.id),
                move |s| {
                    s.saved_permissions.retain(|x| x.id != id);
                    s.send(Command::ForgetSavedPermission {
                        asid: a.clone(),
                        id: id.clone(),
                    });
                },
            );
            let mut card = sub_card(k, format!("sp-{}", p.id)).child(
                row()
                    .w_full()
                    .gap(8.0)
                    .center_y()
                    .child(
                        text(p.action.clone())
                            .font(k.ty.ui_medium)
                            .color(k.c.text_base)
                            .truncate()
                            .grow(1.0)
                            .shrink()
                            .min_w(0.0),
                    )
                    .child(forget),
            );
            if !p.resource.trim().is_empty() {
                card = card.child(
                    row()
                        .w_full()
                        .px(7.0)
                        .py(3.0)
                        .radius(k.radius.sm)
                        .fill(k.c.surface_raised_base)
                        .border(1.0, k.c.border_weak)
                        .child(
                            text(p.resource.clone())
                                .font(k.ty.mono)
                                .color(k.c.text_muted)
                                .truncate()
                                .shrink()
                                .min_w(0.0),
                        ),
                );
            }
            card
        })
        .collect();

    vec![section_card(
        k,
        "saved-card",
        &parts::glyphs::TRUST_WARNING,
        t("Saved permissions"),
        Some(s.saved_permissions.len().to_string()),
        refresh_btn.into_iter().collect(),
        rows,
    )]
}

fn shell_status_color(k: &Kit, status: &str) -> Color {
    match status.to_ascii_lowercase().as_str() {
        "running" | "active" | "started" => k.c.success.fg,
        "failed" | "error" | "killed" => k.c.danger.fg,
        _ => k.c.text_faint,
    }
}

/// Whether the Shells tab should offer "Move turn to background": only while
/// the active session is actually busy.
pub(crate) fn can_background_turn(s: &UiState) -> bool {
    s.selected.is_some() && s.busy()
}

fn shells(s: &UiState, k: &Kit) -> Vec<El> {
    let (agent, dir) = s
        .current()
        .map(|i| (i.agent_id.clone(), i.directory.clone()))
        .unwrap_or_default();
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);

    let refresh_btn = (!agent.is_empty()).then(|| {
        let (a, d) = (agent.clone(), dir.clone());
        small_btn(
            k,
            t("Refresh"),
            ButtonVariant::Outline,
            "shells-refresh".into(),
            move |s| {
                s.send(Command::LoadShells {
                    agent: a.clone(),
                    directory: d.clone(),
                });
            },
        )
    });

    let mut body: Vec<El> = if s.shells.is_empty() {
        vec![
            text(t("No background shells"))
                .font(k.ty.ui_medium)
                .color(k.c.text_muted),
            text(t("Long commands the agent parks keep running here."))
                .font(k.ty.small)
                .color(k.c.text_faint),
        ]
    } else {
        s.shells
            .iter()
            .map(|sh| {
                let (agent, id, dir) = (agent.clone(), sh.id.clone(), dir.clone());
                let confirm_key = format!("kill-sh:{}", sh.id);
                let stop_btn = confirm_btn(
                    s,
                    k,
                    confirm_key,
                    (t("Stop"), t("Confirm stop")),
                    ButtonVariant::Outline,
                    format!("kill-{}", sh.id),
                    move |s| {
                        s.send(Command::KillShell {
                            agent: agent.clone(),
                            id: id.clone(),
                            directory: dir.clone(),
                        });
                    },
                );

                let dot_color = shell_status_color(k, &sh.status);
                let mut meta = row()
                    .gap(6.0)
                    .center_y()
                    .child(
                        el().size(7.0, 7.0)
                            .radius(k.radius.xl)
                            .fill(dot_color)
                            .a11y_hidden(),
                    )
                    .child(
                        text(sh.status.clone())
                            .font(k.ty.small_medium)
                            .color(dot_color),
                    );
                if let Some(pid) = sh.pid {
                    meta = meta.child(
                        Badge::new(format!("PID {pid}"))
                            .variant(BadgeVariant::Subtle)
                            .build(k),
                    );
                }

                sub_card(k, format!("sh-{}", sh.id))
                    .child(
                        row()
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(meta)
                            .child(el().grow(1.0))
                            .child(stop_btn),
                    )
                    .child(
                        text(timeline::clip(&sh.command, 100))
                            .font(k.ty.mono)
                            .color(k.c.text_base)
                            .truncate(),
                    )
            })
            .collect()
    };

    if can_background_turn(s) {
        if let Some(asid) = s.selected.clone() {
            body.push(row().w_full().pt(4.0).child(small_btn(
                k,
                t("Move turn to background"),
                ButtonVariant::Outline,
                "background".into(),
                move |s| {
                    s.send(Command::BackgroundSession {
                        asid: AgentSessionId(asid.clone()),
                    });
                },
            )));
        }
    }

    vec![section_card(
        k,
        "shells-card",
        &parts::glyphs::TERMINAL,
        t("Background shells"),
        (!s.shells.is_empty()).then(|| s.shells.len().to_string()),
        refresh_btn.into_iter().collect(),
        body,
    )]
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let mut strip = row()
        .key("inspector-tabs")
        .role("tablist")
        .label(tr_ref(language, "Inspector"))
        .w_full()
        .h(TOOLBAR_H)
        .px(8.0)
        .gap(3.0)
        .center_y()
        .focus_group();
    for (label, tab) in TABS {
        strip = strip.child(tab_button(
            k,
            tab_label(language, label),
            tab_count(s, tab),
            tab,
            s.inspector_tab == tab,
        ));
    }
    let body = match s.inspector_tab {
        InspectorTab::Queue => queue(s, k),
        InspectorTab::Git => git(s, k),
        InspectorTab::Subagents => super::session_tree::view(s, k),
        InspectorTab::Saved => saved(s, k),
        InspectorTab::Shells => shells(s, k),
    };
    col()
        .key("inspector")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(strip)
        .child(el().w_full().h(1.0).fill(k.c.border_weak).a11y_hidden())
        .child(
            col()
                .key("inspector-body")
                .items_start()
                .w_full()
                .gap(10.0)
                .px(10.0)
                .pt(10.0)
                .pb(14.0)
                .children(body)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .grow(1.0)
                .min_h(0.0)
                .scroll_bar(k),
        )
}

fn commit_changes(s: &mut UiState, push: bool) {
    if !can_run_local_git(s) {
        return;
    }
    let msg = s.git_commit_msg.text.trim().to_string();
    if msg.is_empty() {
        let lang = s.cfg.general.language;
        s.toast(tr_ref(lang, "Commit message cannot be empty"), None);
        return;
    }
    let cwd = s
        .current()
        .and_then(|info| info.directory.clone())
        .unwrap_or_else(|| s.terminal_cwd().display().to_string());
    if cwd.is_empty() {
        return;
    }
    s.git_commit_msg = rune::TextEdit::new("");
    let asid = s.selected.clone().map(AgentSessionId);
    s.send(Command::GitCommit {
        cwd,
        message: msg,
        push,
        asid,
    });
}

fn push_changes(s: &mut UiState) {
    if !can_run_local_git(s) {
        return;
    }
    let cwd = s
        .current()
        .and_then(|info| info.directory.clone())
        .unwrap_or_else(|| s.terminal_cwd().display().to_string());
    if cwd.is_empty() {
        return;
    }
    let asid = s.selected.clone().map(AgentSessionId);
    s.send(Command::GitPush { cwd, asid });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{SavedPermission, ShellRow, WorktreeRow};
    use crate::model::compose::{BranchInfo, InboxItem};
    use crate::model::inbox::Update;
    use muqun_gateway::agents::{AgentSessionInfo, AgentSessionStatus};

    fn mk_session(asid: &str, status: &str, dir: Option<&str>) -> AgentSessionInfo {
        serde_json::from_value(serde_json::json!({
            "asid": asid,
            "agent_id": "opencode",
            "title": asid,
            "status": status,
            "directory": dir,
            "updated_ms": 1000u64,
        }))
        .unwrap()
    }

    #[test]
    fn subagents_renders_items_and_tile_all() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let root: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "root-1",
            "agent_id": "opencode",
            "title": "Main session",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        let sub1: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "sub-1",
            "agent_id": "opencode",
            "parent_id": "root-1",
            "title": "Fix bug in parser",
            "status": "busy",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        let sub2: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "sub-2",
            "agent_id": "deepseek",
            "parent_id": "root-1",
            "title": "Add tests for API",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        s.sessions = vec![root];
        s.selected = Some("root-1".into());
        s.descendants = vec![sub1, sub2];
        let k = Kit::neutral(s.dark);
        let els = crate::ui::session_tree::view(&s, &k);
        assert!(els.len() >= 2);
    }

    #[test]
    fn tab_counts_cover_all_five_inspector_tabs() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![mk_session("s1", "idle", Some("/repo"))];
        s.selected = Some("s1".into());

        // Initially all counts are None.
        for (_, tab) in TABS {
            assert_eq!(tab_count(&s, tab), None);
        }

        s.queue.push(InboxItem {
            id: "q1".into(),
            text: "follow up".into(),
            steer: false,
            files: vec![],
        });
        s.diff_stat.files = 3;
        s.descendants = vec![mk_session("sub-1", "busy", Some("/repo"))];
        s.saved_permissions = vec![
            SavedPermission {
                id: "p1".into(),
                project_id: "proj".into(),
                action: "bash".into(),
                resource: "cargo test".into(),
            },
            SavedPermission {
                id: "p2".into(),
                project_id: "proj".into(),
                action: "read".into(),
                resource: "src/**".into(),
            },
        ];
        s.shells = vec![ShellRow {
            id: "sh-1".into(),
            command: "npm run dev".into(),
            status: "running".into(),
            pid: Some(4242),
        }];

        assert_eq!(tab_count(&s, InspectorTab::Queue), Some(1));
        assert_eq!(tab_count(&s, InspectorTab::Git), Some(3));
        assert_eq!(tab_count(&s, InspectorTab::Subagents), Some(1));
        assert_eq!(tab_count(&s, InspectorTab::Saved), Some(2));
        assert_eq!(tab_count(&s, InspectorTab::Shells), Some(1));

        // Tab strip renames Sub-agents -> Tree.
        assert_eq!(tab_label(Language::En, "Tree"), "Tree");
        assert_eq!(tab_label(Language::ZhCn, "Tree"), "会话树");
        let k = Kit::neutral(s.dark);
        let _ = view(&s, &k);
    }

    #[test]
    fn git_inspector_renders_commit_box_when_changes_present_on_git_repo() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![mk_session("test-session", "idle", Some("/tmp/repo"))];
        s.selected = Some("test-session".into());
        s.branches.insert(
            "/tmp/repo".into(),
            BranchInfo {
                current: Some("feat/ui".into()),
                local: vec!["main".into(), "feat/ui".into()],
                remote_url: Some("https://github.com/muqun/desktop".into()),
                remote_host: Some(GitRemoteHost::GitHub),
            },
        );
        s.diff_stat.files = 2;
        s.diff_stat.paths = vec![crate::model::types::DiffFile {
            path: "src/main.rs".into(),
            added: 5,
            removed: 1,
            patch: "".into(),
        }];
        s.git_commit_msg = rune::TextEdit::new("feat: cool feature");
        assert!(can_run_local_git(&s));
        let k = Kit::neutral(s.dark);
        // Branch + Changes + Commit & Push + Worktrees = 4 cards.
        let els = git(&s, &k);
        assert_eq!(els.len(), 4);

        while rx.try_recv().is_ok() {}

        commit_changes(&mut s, false);
        assert!(s.git_commit_msg.text.is_empty());
        let cmd = rx.try_recv().unwrap();
        match cmd {
            Command::GitCommit { message, push, .. } => {
                assert_eq!(message, "feat: cool feature");
                assert!(!push);
            }
            other => panic!("expected GitCommit command, got {:?}", other),
        }
    }

    #[test]
    fn git_inspector_hides_push_and_commit_on_non_git_directory() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![mk_session("non-git", "idle", Some("/tmp/plain-folder"))];
        s.selected = Some("non-git".into());
        s.branches.insert(
            "/tmp/plain-folder".into(),
            BranchInfo {
                current: None,
                local: vec![],
                remote_url: None,
                remote_host: None,
            },
        );

        let k = Kit::neutral(s.dark);
        assert!(!can_run_local_git(&s));
        assert!(
            !should_show_push_commits(&s),
            "non-git repo must hide Push commits"
        );
        // Clean non-git folder renders 3 cards (No repo + Changes + Worktrees).
        assert_eq!(git(&s, &k).len(), 3);

        // Even if diff_stat has entries, non-git folder hides local Commit & Push card.
        s.diff_stat.files = 1;
        s.diff_stat.paths = vec![crate::model::types::DiffFile {
            path: "notes.txt".into(),
            added: 2,
            removed: 0,
            patch: "".into(),
        }];
        assert_eq!(git(&s, &k).len(), 3);

        // Calling push_changes or commit_changes on a non-git dir must be a no-op.
        while rx.try_recv().is_ok() {}
        s.git_commit_msg = rune::TextEdit::new("should not run");
        commit_changes(&mut s, false);
        push_changes(&mut s);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn git_inspector_handles_remote_session_and_show_all_toggle() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.cfg.current_computer = "mac-mini".into();
        s.sessions = vec![mk_session("remote-1", "idle", Some("/Users/dev/app"))];
        s.selected = Some("remote-1".into());
        s.diff_stat.files = 15;
        s.diff_stat.paths = (0..15)
            .map(|i| crate::model::types::DiffFile {
                path: format!("src/file_{i}.rs"),
                added: 1,
                removed: 0,
                patch: "".into(),
            })
            .collect();

        assert!(s.is_remote_session());
        assert!(!can_run_local_git(&s));
        assert_eq!(visible_diff_file_count(&s), INITIAL_DIFF_FILES);

        let k = Kit::neutral(s.dark);
        // Remote session renders 3 cards (Remote host + Changes + Worktrees, no local Commit card).
        assert_eq!(git(&s, &k).len(), 3);

        s.inspector_git_show_all = true;
        assert_eq!(visible_diff_file_count(&s), 15);
        assert_eq!(git(&s, &k).len(), 3);

        // Refreshing Git on a remote session must NOT dispatch local ComposeCommand::Branch.
        while rx.try_recv().is_ok() {}
        s.inspector_tab = InspectorTab::Git;
        s.refresh_inspector();
        let sent: Vec<Command> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !sent
                .iter()
                .any(|c| matches!(c, Command::Compose(ComposeCommand::Branch { .. }))),
            "remote session must not run local Branch lookup: {sent:?}"
        );
        assert!(sent.iter().any(|c| matches!(c, Command::RefreshDiff(_))));
    }

    #[test]
    fn shells_hides_move_turn_to_background_when_idle_and_shows_when_busy() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![mk_session("s1", "idle", Some("/repo"))];
        s.selected = Some("s1".into());
        let k = Kit::neutral(s.dark);

        assert!(
            !can_background_turn(&s),
            "idle session must hide Move turn to background"
        );
        assert_eq!(shells(&s, &k).len(), 1);

        s.sessions[0].status = AgentSessionStatus::Busy;
        assert!(
            can_background_turn(&s),
            "busy session should show Move turn to background"
        );
        assert_eq!(shells(&s, &k).len(), 1);
    }

    #[test]
    fn switching_sessions_clears_and_guards_stale_shells_and_worktrees() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions = vec![
            mk_session("s1", "idle", Some("/repo-a")),
            mk_session("s2", "idle", Some("/repo-b")),
        ];
        s.selected = Some("s1".into());
        s.shells = vec![ShellRow {
            id: "sh-a".into(),
            command: "cargo watch".into(),
            status: "running".into(),
            pid: Some(1234),
        }];
        s.worktrees = vec![WorktreeRow {
            directory: "/repo-a-wt".into(),
            strategy: Some("linked".into()),
        }];

        s.open_session("s2");
        assert!(s.shells.is_empty());
        assert!(s.worktrees.is_empty());

        // Stale updates from the previous session arriving before s2 refreshes are ignored.
        s.apply_updates(vec![
            Update::Shells(vec![ShellRow {
                id: "sh-stale".into(),
                command: "old".into(),
                status: "running".into(),
                pid: None,
            }]),
            Update::Worktrees(vec![WorktreeRow {
                directory: "/repo-a-wt".into(),
                strategy: Some("linked".into()),
            }]),
        ]);
        assert!(s.shells.is_empty());
        assert!(s.worktrees.is_empty());

        // Once s2 refreshes its inspector tab, fresh responses are accepted.
        s.inspector_tab = InspectorTab::Shells;
        s.refresh_inspector();
        s.apply_updates(vec![Update::Shells(vec![ShellRow {
            id: "sh-b".into(),
            command: "npm test".into(),
            status: "running".into(),
            pid: Some(999),
        }])]);
        assert_eq!(s.shells.len(), 1);
        assert_eq!(s.shells[0].id, "sh-b");
    }
}
