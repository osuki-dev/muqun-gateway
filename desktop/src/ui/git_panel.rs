//! First-class Developer Git Workbench panel (local & remote unified) and
//! shared per-tile inline drawer chrome.
//!
//! Can be rendered either:
//! - Directly inside a session window / split tile via [`tile_inline_drawer`]
//!   (bound strictly to that tile's `asid` so every tile has its own independent
//!   Git / Activity drawer right inside its own window!), or
//! - Standalone in the Dock / Tile / Page via [`view`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use muqun_gateway::agents::AgentSessionId;
use rune::{col, el, row, text, El, Icon, ScrollSpec, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit,
    Menu, MenuItem, ScrollBar, TextInput,
};

use super::copy::{tr, tr_ref};
use super::{icons, parts, timeline, Mode, OpenMenu, PanelTarget, TileInlinePanel, UiState};
use crate::hub::{
    GitChangedFile, GitChangedFiles, GitCommitRow, GitCommitsPage, GitPrRow, GitPrsPage,
    GitRepoOverview, WorktreeRow,
};
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, GitRemoteHost};

const INITIAL_FILES_LIMIT: usize = 15;
const FILES_PAGE_STEP: usize = 20;
const COMMITS_PAGE_SIZE: usize = 10;
const PRS_PAGE_SIZE: u32 = 10;
const PR_AUTO_LOAD_COOLDOWN: Duration = Duration::from_millis(900);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitDiffMode {
    #[default]
    Working,
    Branch,
}

impl GitDiffMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Branch => "branch",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GitPanelState {
    pub target_asid: Option<String>,
    pub diff_mode: GitDiffMode,
    pub overview: Option<GitRepoOverview>,
    pub overview_error: Option<String>,
    pub overview_loading: bool,
    pub files: GitChangedFiles,
    pub files_limit: usize,
    pub pr_state: &'static str,
    pub pr_author: &'static str,
    pub prs: Vec<GitPrRow>,
    pub pr_page: u32,
    pub pr_has_more: bool,
    pub pr_loading: bool,
    pub pr_host: String,
    pub pr_supported: Option<bool>,
    pub pr_reason: Option<String>,
    pub pr_repo_url: Option<String>,
    pub pr_last_auto_load: Option<Instant>,
    pub commits_open: bool,
    pub commits: Vec<GitCommitRow>,
    pub commits_offset: usize,
    pub commits_has_more: bool,
    pub commits_loading: bool,
    pub worktrees_open: bool,
    pub confirm: Option<String>,
    pub busy_action: Option<String>,
}

impl Default for GitPanelState {
    fn default() -> Self {
        Self {
            target_asid: None,
            diff_mode: GitDiffMode::Working,
            overview: None,
            overview_error: None,
            overview_loading: false,
            files: GitChangedFiles::default(),
            files_limit: INITIAL_FILES_LIMIT,
            pr_state: "open",
            pr_author: "all",
            prs: Vec::new(),
            pr_page: 0,
            pr_has_more: false,
            pr_loading: false,
            pr_host: String::new(),
            pr_supported: None,
            pr_reason: None,
            pr_repo_url: None,
            pr_last_auto_load: None,
            commits_open: false,
            commits: Vec::new(),
            commits_offset: 0,
            commits_has_more: false,
            commits_loading: false,
            worktrees_open: false,
            confirm: None,
            busy_action: None,
        }
    }
}

impl GitPanelState {
    pub fn apply_overview(
        &mut self,
        asid: &str,
        overview: Result<GitRepoOverview, String>,
        files: Result<GitChangedFiles, String>,
    ) {
        if self.target_asid.as_deref().is_some_and(|t| t != asid) {
            return;
        }
        self.target_asid = Some(asid.to_string());
        self.overview_loading = false;
        match overview {
            Ok(ov) => {
                if ov.worktrees.len() > 1 {
                    self.worktrees_open = true;
                }
                self.overview = Some(ov);
                self.overview_error = None;
            }
            Err(err) => {
                self.overview_error = Some(err);
            }
        }
        if let Ok(f) = files {
            self.files = f;
        }
    }

    pub fn apply_commits(
        &mut self,
        asid: &str,
        append: bool,
        result: Result<GitCommitsPage, String>,
    ) {
        if self.target_asid.as_deref().is_some_and(|t| t != asid) {
            return;
        }
        self.commits_loading = false;
        if let Ok(page) = result {
            if append {
                for row in page.items {
                    if !self.commits.iter().any(|c| c.hash == row.hash) {
                        self.commits.push(row);
                    }
                }
            } else {
                self.commits = page.items;
            }
            self.commits_offset = page.next_offset.unwrap_or(self.commits.len());
            self.commits_has_more = page.has_more;
        }
    }

    pub fn apply_prs(
        &mut self,
        asid: &str,
        state: &str,
        author: &str,
        append: bool,
        result: Result<GitPrsPage, String>,
    ) {
        if self.target_asid.as_deref().is_some_and(|t| t != asid)
            || self.pr_state != state
            || self.pr_author != author
        {
            return;
        }
        self.pr_loading = false;
        match result {
            Ok(page) => {
                self.pr_host = page.host;
                self.pr_supported = Some(page.supported);
                self.pr_reason = page.reason;
                self.pr_repo_url = page.repo_url;
                if append {
                    for row in page.items {
                        if !self.prs.iter().any(|p| p.number == row.number) {
                            self.prs.push(row);
                        }
                    }
                } else {
                    self.prs = page.items;
                }
                self.pr_page = page.page.max(1);
                self.pr_has_more = page.has_more;
            }
            Err(err) => {
                self.pr_supported = Some(false);
                self.pr_reason = Some(err);
                self.pr_has_more = false;
            }
        }
    }
}

pub(crate) fn small_btn(
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

pub(crate) fn section_card(
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
            .child(el().w_full().h(1.0).fill(k.c.border_muted).a11y_hidden())
            .child(col().w_full().px(12.0).py(12.0).gap(8.0).children(body));
    }
    card
}

pub(crate) fn sub_card(k: &Kit, key: impl Into<String>) -> El {
    col()
        .key(key.into())
        .w_full()
        .px(10.0)
        .py(10.0)
        .gap(6.0)
        .radius(k.radius.md)
        .fill(k.c.layer_01.alpha(0.55))
        .border(1.0, k.c.border_weak.alpha(0.75))
}

pub(crate) fn short_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

#[allow(clippy::too_many_arguments)]
fn confirm_btn(
    s: &UiState,
    k: &Kit,
    asid: &str,
    confirm_id: String,
    labels: (&str, &str),
    idle_variant: ButtonVariant,
    key: String,
    on_confirm: impl Fn(&mut UiState) + 'static,
) -> El {
    let armed = s.git_panel_for(asid).confirm.as_deref() == Some(confirm_id.as_str());
    let label = if armed { labels.1 } else { labels.0 };
    let variant = if armed {
        ButtonVariant::Danger
    } else {
        idle_variant
    };
    let asid_owned = asid.to_string();
    small_btn(k, label, variant, key, move |s| {
        if s.git_panel_for(&asid_owned).confirm.as_deref() == Some(confirm_id.as_str()) {
            s.git_panel_mut(&asid_owned).confirm = None;
            on_confirm(s);
        } else {
            s.git_panel_mut(&asid_owned).confirm = Some(confirm_id.clone());
        }
    })
}

/// Shared 30px Target Session Bar used only in standalone Dock/Tile/Page views.
pub(crate) fn target_session_bar(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    key_prefix: &str,
) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let target_asid = s.panel_target_asid().map(str::to_string);
    let pinned = matches!(s.panel_target, PanelTarget::Pinned(_));
    let info = s.panel_target_session();
    let host_id = s.panel_target_computer();
    let host_label = if UiState::is_remote_computer_id(&host_id) {
        Some(s.computer_name(&host_id))
    } else {
        None
    };

    let title = info
        .map(|i| {
            let trimmed = i.title.trim();
            if trimmed.is_empty() {
                tr(language, "Untitled session")
            } else {
                trimmed.to_string()
            }
        })
        .unwrap_or_else(|| tr(language, "No active session"));

    let open_tiles = s.open_tile_sessions();
    let open = s.menu == Some(OpenMenu::PanelTargetPicker);
    let can_pick = !open_tiles.is_empty();

    let agent_dot_color = info
        .map(|i| super::agents::color(&i.agent_id, s.dark))
        .unwrap_or(k.c.icon_muted);

    let mut label_row = row()
        .key(format!("{key_prefix}-target-trigger"))
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h(26.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.md)
        .fill(k.c.layer_01.alpha(0.65))
        .border(
            1.0,
            if open {
                k.c.bg_accent
            } else {
                k.c.border_weak.alpha(0.8)
            },
        )
        .child(
            el().size(6.0, 6.0)
                .radius(3.0)
                .fill(agent_dot_color)
                .a11y_hidden(),
        )
        .child(
            text(title)
                .font(k.ty.small_medium)
                .color(k.c.text_base)
                .truncate()
                .grow(1.0)
                .shrink()
                .min_w(0.0),
        );
    if let Some(host) = host_label {
        label_row = label_row.child(Badge::new(host).variant(BadgeVariant::Subtle).build(k));
    }
    if can_pick {
        label_row = label_row.child(
            text(if open { "▴" } else { "▾" })
                .font(k.ty.small)
                .color(k.c.text_muted)
                .a11y_hidden(),
        );
    }

    let picker = if can_pick {
        let trigger = k
            .interactive(label_row)
            .role("button")
            .label(t("Select target session"))
            .expanded(open)
            .on_press(move |s: &mut UiState| {
                s.menu = (!open).then_some(OpenMenu::PanelTargetPicker);
            });
        let mut menu = Menu::new(format!("{key_prefix}-target-menu"))
            .trigger(trigger)
            .open(open)
            .min_width(220.0)
            .on_close(|s: &mut UiState| s.menu = None);
        menu = menu.item(
            MenuItem::new(t("Follow focused tile"))
                .checked(!pinned)
                .on_select(|s: &mut UiState| {
                    s.menu = None;
                    s.set_panel_target(PanelTarget::FollowFocused);
                }),
        );
        menu = menu.separator();
        for (idx, asid, tile_title) in open_tiles {
            let is_selected = pinned && target_asid.as_deref() == Some(asid.as_str());
            let label = format!("Tile {} · {}", idx + 1, timeline::clip(&tile_title, 28));
            let target_id = asid.clone();
            menu = menu.item(MenuItem::new(label).checked(is_selected).on_select(
                move |s: &mut UiState| {
                    s.menu = None;
                    s.set_panel_target(PanelTarget::Pinned(target_id.clone()));
                },
            ));
        }
        menu.build(k, layer).grow(1.0).shrink().min_w(0.0)
    } else {
        label_row
    };

    let pin_label = if pinned { t("Pinned") } else { t("Pin") };
    let pin_btn = Button::new(pin_label)
        .variant(if pinned {
            ButtonVariant::Neutral
        } else {
            ButtonVariant::GhostMuted
        })
        .control_size(ControlSize::Small)
        .disabled(target_asid.is_none())
        .on_press(|s: &mut UiState| s.toggle_panel_pin())
        .build(k)
        .key(format!("{key_prefix}-target-pin"));

    col()
        .key(format!("{key_prefix}-target-bar"))
        .w_full()
        .child(
            row()
                .w_full()
                .px(12.0)
                .py(6.0)
                .gap(6.0)
                .center_y()
                .child(picker)
                .child(pin_btn),
        )
        .child(el().w_full().h(1.0).fill(k.c.border_muted).a11y_hidden())
}

fn filter_pill(
    k: &Kit,
    label: &str,
    active: bool,
    key: String,
    on_press: impl Fn(&mut UiState) + 'static,
) -> El {
    let mut b = row()
        .key(key)
        .h(22.0)
        .px(8.0)
        .center()
        .radius(k.radius.sm)
        .role("tab")
        .label(label.to_string())
        .focusable(true)
        .focus_selected(active)
        .child(
            text(label.to_string())
                .font(if active {
                    k.ty.small_medium
                } else {
                    k.ty.small
                })
                .color(if active {
                    k.c.text_base
                } else {
                    k.c.text_muted
                })
                .a11y_hidden(),
        );
    if active {
        b = b.fill(k.c.surface_raised_base).border(1.0, k.c.border_weak);
    } else {
        b = b.fill(k.c.layer_01.alpha(0.55));
    }
    k.interactive(b).on_press(on_press)
}

/// Inline drawer rendered directly inside a session window or split tile, right beside
/// that tile's chat timeline and composer. Keeps each tile's Git / Activity strictly
/// scoped to that tile with zero cross-tile focus interference.
pub(crate) fn tile_inline_drawer(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    tile_idx: usize,
    asid: &str,
    panel: TileInlinePanel,
    width: f32,
) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let git_count = {
        let gp = s.git_panel_for(asid);
        if !gp.files.files.is_empty() {
            gp.files.files.len()
        } else if s.selected.as_deref() == Some(asid) {
            s.diff_stat.files
        } else {
            0
        }
    };
    let act_count = super::activity_panel::badge_count_for(s, asid);

    let git_label = if git_count > 0 {
        format!("Git ({git_count})")
    } else {
        "Git".to_string()
    };
    let act_label = if act_count > 0 {
        format!("{} ({act_count})", t("Activity"))
    } else {
        t("Activity").to_string()
    };

    let header = row()
        .key(format!("tile-inline-header-{tile_idx}"))
        .w_full()
        .h(34.0)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .child(filter_pill(
            k,
            &git_label,
            panel == TileInlinePanel::Git,
            format!("tile-inline-tab-git-{tile_idx}"),
            move |s| s.set_tile_inline_panel(tile_idx, Some(TileInlinePanel::Git)),
        ))
        .child(filter_pill(
            k,
            &act_label,
            panel == TileInlinePanel::Activity,
            format!("tile-inline-tab-activity-{tile_idx}"),
            move |s| s.set_tile_inline_panel(tile_idx, Some(TileInlinePanel::Activity)),
        ))
        .child(el().grow(1.0))
        .child(
            IconButton::new(k.glyph.close)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(t("Close"))
                .help(t("Close"))
                .on_press(move |s: &mut UiState| s.set_tile_inline_panel(tile_idx, None))
                .build(k)
                .key(format!("tile-inline-close-{tile_idx}")),
        );

    let content = match panel {
        TileInlinePanel::Git => view_for_session(s, k, layer, asid, true),
        TileInlinePanel::Activity => {
            super::activity_panel::view_for_session(s, k, layer, asid, true)
        }
    };

    row()
        .key(format!("tile-inline-drawer-{tile_idx}"))
        .h_full()
        .min_h(0.0)
        .w(width)
        .min_w(240.0)
        .shrink()
        .child(el().w(1.0).h_full().fill(k.c.border_muted).a11y_hidden())
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .min_h(0.0)
                .fill(k.c.bg_base.alpha(0.32))
                .child(header)
                .child(el().w_full().h(1.0).fill(k.c.border_muted).a11y_hidden())
                .child(content.grow(1.0).min_h(0.0)),
        )
}

#[allow(clippy::too_many_arguments)]
fn branch_switcher_dropdown(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    asid: &str,
    cur_branch: &str,
    local: &[String],
    use_gateway_vcs: bool,
    cwd: Option<&str>,
) -> El {
    let language = s.cfg.general.language;
    let open = s.menu == Some(OpenMenu::GitBranch);
    let can_switch = !local.is_empty();
    let chevron = if open { "▴" } else { "▾" };

    let mut trigger = row()
        .key(format!("git-branch-switcher-trigger-{asid}"))
        .w_full()
        .h(30.0)
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
    if !can_switch {
        return trigger;
    }
    trigger = k
        .interactive(trigger)
        .expanded(open)
        .on_press(move |s: &mut UiState| {
            s.menu = (!open).then_some(OpenMenu::GitBranch);
        });
    let mut menu = Menu::new(format!("git-branch-switcher-{asid}"))
        .trigger(trigger)
        .open(open)
        .min_width(220.0)
        .on_close(|s: &mut UiState| s.menu = None);
    for branch in local.iter().take(60) {
        let is_current = branch == cur_branch;
        let target_branch = branch.clone();
        let asid_owned = asid.to_string();
        let cwd_owned = cwd.unwrap_or_default().to_string();
        menu = menu.item(MenuItem::new(branch.clone()).checked(is_current).on_select(
            move |s: &mut UiState| {
                s.menu = None;
                if is_current {
                    return;
                }
                if use_gateway_vcs && !asid_owned.is_empty() {
                    let mode = {
                        let gp = s.git_panel_mut(&asid_owned);
                        gp.busy_action = Some("checkout".into());
                        gp.diff_mode.as_str().to_string()
                    };
                    s.send(Command::VcsCheckout {
                        asid: AgentSessionId(asid_owned.clone()),
                        branch: Some(target_branch.clone()),
                        pr_number: None,
                        mode,
                    });
                } else if !cwd_owned.is_empty() {
                    s.send(Command::Compose(ComposeCommand::Checkout {
                        cwd: cwd_owned.clone(),
                        branch: target_branch.clone(),
                    }));
                }
            },
        ));
    }
    menu.build(k, layer).w_full()
}

fn format_relative_time(ts_ms: i64) -> String {
    if ts_ms <= 0 {
        return String::new();
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff_s = ((now_ms - ts_ms) / 1000).max(0);
    if diff_s < 60 {
        format!("{diff_s}s")
    } else if diff_s < 3600 {
        format!("{}m", diff_s / 60)
    } else if diff_s < 86_400 {
        format!("{}h", diff_s / 3600)
    } else {
        format!("{}d", diff_s / 86_400)
    }
}

#[allow(clippy::too_many_arguments)]
fn file_row_view(
    s: &UiState,
    k: &Kit,
    asid: &str,
    file: &GitChangedFile,
    idx: usize,
    staged_group: bool,
    allow_stage_ops: bool,
    allow_discard: bool,
) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let path_for_diff = file.path.clone();
    let asid_for_diff = asid.to_string();
    let diff_btn = small_btn(
        k,
        t("Diff"),
        ButtonVariant::Outline,
        format!("git-diff-{asid}-{staged_group}-{idx}"),
        move |s| {
            if !asid_for_diff.is_empty() {
                if s.selected.as_deref() != Some(asid_for_diff.as_str()) {
                    if let Some(tile_idx) = s.tile_of(&asid_for_diff) {
                        s.focus_tile(tile_idx);
                    }
                }
                s.send(Command::OpenDiff {
                    asid: AgentSessionId(asid_for_diff.clone()),
                    file: Some(path_for_diff.clone()),
                });
            }
            s.mode = Mode::Session;
            s.show_diff = true;
        },
    );

    let mut actions = row().gap(4.0).center_y();
    if allow_stage_ops {
        let p = file.path.clone();
        let a = asid.to_string();
        if staged_group {
            actions = actions.child(small_btn(
                k,
                "−",
                ButtonVariant::GhostMuted,
                format!("git-unstage-{asid}-{idx}"),
                move |s| {
                    let mode = s.git_panel_for(&a).diff_mode.as_str().to_string();
                    s.send(Command::VcsUnstage {
                        asid: AgentSessionId(a.clone()),
                        paths: vec![p.clone()],
                        all: false,
                        mode,
                    });
                },
            ));
        } else {
            actions = actions.child(small_btn(
                k,
                "+",
                ButtonVariant::GhostMuted,
                format!("git-stage-{asid}-{idx}"),
                move |s| {
                    let mode = s.git_panel_for(&a).diff_mode.as_str().to_string();
                    s.send(Command::VcsStage {
                        asid: AgentSessionId(a.clone()),
                        paths: vec![p.clone()],
                        all: false,
                        mode,
                    });
                },
            ));
        }
    }
    actions = actions.child(diff_btn);
    if allow_discard && !staged_group {
        let p = file.path.clone();
        let a = asid.to_string();
        let confirm_key = format!("discard:{p}");
        actions = actions.child(confirm_btn(
            s,
            k,
            asid,
            confirm_key,
            (t("Discard"), t("Confirm")),
            ButtonVariant::GhostMuted,
            format!("git-discard-{asid}-{idx}"),
            move |s| {
                let mode = s.git_panel_for(&a).diff_mode.as_str().to_string();
                s.send(Command::VcsDiscard {
                    asid: AgentSessionId(a.clone()),
                    path: p.clone(),
                    mode,
                });
            },
        ));
    }

    let status_badge = match file.status.as_str() {
        "added" => "A",
        "deleted" => "D",
        "renamed" => "R",
        "untracked" => "U",
        "conflicted" => "!",
        _ => "M",
    };
    let status_color = match file.status.as_str() {
        "added" | "untracked" => k.c.success.fg,
        "deleted" | "conflicted" => k.c.danger.fg,
        _ => k.c.warning.fg,
    };

    let mut stats = row().gap(6.0).center_y().child(
        text(status_badge.to_string())
            .font(k.ty.small_medium)
            .color(status_color),
    );
    if file.binary {
        stats = stats.child(text("bin").font(k.ty.small).color(k.c.text_faint));
    } else {
        if let Some(a) = file.added.filter(|&n| n > 0) {
            stats = stats.child(text(format!("+{a}")).font(k.ty.small).color(k.c.success.fg));
        }
        if let Some(r) = file.removed.filter(|&n| n > 0) {
            stats = stats.child(text(format!("−{r}")).font(k.ty.small).color(k.c.danger.fg));
        }
    }

    row()
        .key(format!("gf-{asid}-{staged_group}-{}", file.path))
        .w_full()
        .gap(8.0)
        .center_y()
        .px(10.0)
        .py(6.0)
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
                    text(file.path.clone())
                        .font(k.ty.small_medium)
                        .color(k.c.text_base)
                        .truncate(),
                )
                .child(stats),
        )
        .child(actions)
}

fn commit_changes(s: &mut UiState, asid: &str, push: bool, stage_all: bool) {
    let msg = s.git_commit_msg.text.trim().to_string();
    if msg.is_empty() || asid.is_empty() {
        return;
    }
    s.git_commit_msg.clear();
    let mode = {
        let gp = s.git_panel_mut(asid);
        gp.busy_action = Some(if push { "commit_push" } else { "commit" }.into());
        gp.diff_mode.as_str().to_string()
    };
    s.send(Command::VcsCommit {
        asid: AgentSessionId(asid.to_string()),
        message: msg,
        stage_all,
        push,
        mode,
    });
}

fn pull_requests_card(s: &UiState, k: &Kit, asid: &str, repo_url: Option<&str>) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let gp = s.git_panel_for(asid);

    let mut trailing = Vec::new();
    if let Some(url) = gp.pr_repo_url.as_deref().or(repo_url) {
        let host = GitRemoteHost::detect(url);
        if host != GitRemoteHost::Remote {
            let cur_branch = gp.overview.as_ref().and_then(|o| o.branch.as_deref());
            let pr_url = host.pr_url(url, cur_branch);
            trailing.push(small_btn(
                k,
                t("New PR"),
                ButtonVariant::Outline,
                format!("git-prs-new-{asid}"),
                move |_| crate::platform::shell::open_url(&pr_url),
            ));
        }
    }
    let refresh_asid = asid.to_string();
    trailing.push(
        IconButton::new(&icons::REFRESH)
            .variant(IconButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .label(t("Refresh"))
            .help(t("Refresh"))
            .on_press(move |s: &mut UiState| {
                s.request_git_prs(&refresh_asid, 1, true);
            })
            .build(k)
            .key(format!("git-prs-refresh-{asid}")),
    );

    let asid_open = asid.to_string();
    let asid_mine = asid.to_string();
    let asid_all = asid.to_string();
    let filter_bar = row()
        .key(format!("git-prs-filters-{asid}"))
        .w_full()
        .gap(4.0)
        .center_y()
        .child(filter_pill(
            k,
            t("Open"),
            gp.pr_state == "open" && gp.pr_author == "all",
            format!("pr-filter-open-{asid}"),
            move |s| {
                let gp = s.git_panel_mut(&asid_open);
                gp.pr_state = "open";
                gp.pr_author = "all";
                s.request_git_prs(&asid_open, 1, false);
            },
        ))
        .child(filter_pill(
            k,
            t("Mine"),
            gp.pr_author == "me",
            format!("pr-filter-mine-{asid}"),
            move |s| {
                let gp = s.git_panel_mut(&asid_mine);
                gp.pr_state = "open";
                gp.pr_author = "me";
                s.request_git_prs(&asid_mine, 1, false);
            },
        ))
        .child(filter_pill(
            k,
            t("All"),
            gp.pr_state == "all" && gp.pr_author == "all",
            format!("pr-filter-all-{asid}"),
            move |s| {
                let gp = s.git_panel_mut(&asid_all);
                gp.pr_state = "all";
                gp.pr_author = "all";
                s.request_git_prs(&asid_all, 1, false);
            },
        ));

    let mut body = vec![filter_bar];

    if gp.pr_loading && gp.prs.is_empty() {
        body.push(
            text(t("Loading pull requests…"))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    } else if gp.pr_supported == Some(false) {
        let msg = gp
            .pr_reason
            .clone()
            .unwrap_or_else(|| tr(language, "PR listing unavailable for this repository."));
        body.push(text(msg).font(k.ty.small).color(k.c.text_faint));
        if let Some(url) = gp.pr_repo_url.as_deref().or(repo_url) {
            let host = GitRemoteHost::detect(url);
            let prs_url = host.pr_url(url, None);
            body.push(row().child(small_btn(
                k,
                t("Open PRs in browser"),
                ButtonVariant::Outline,
                format!("git-prs-open-web-{asid}"),
                move |_| crate::platform::shell::open_url(&prs_url),
            )));
        }
    } else if gp.prs.is_empty() {
        body.push(
            text(t("No matching pull requests"))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    } else {
        for pr in &gp.prs {
            let state_label = if pr.draft {
                "Draft"
            } else {
                match pr.state.as_str() {
                    "merged" => "Merged",
                    "closed" => "Closed",
                    _ => "Open",
                }
            };
            let state_variant = if pr.draft {
                BadgeVariant::Subtle
            } else if pr.state == "open" {
                BadgeVariant::Accent
            } else {
                BadgeVariant::Subtle
            };
            let open_url = pr.url.clone();
            let checkout_asid = asid.to_string();
            let pr_num = pr.number;
            let mut top_row = row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(
                    text(format!("#{}", pr.number))
                        .font(k.ty.mono)
                        .color(k.c.text_accent),
                )
                .child(Badge::new(state_label).variant(state_variant).build(k));
            if let Some(ci) = pr.checks_status.as_deref() {
                let dot_color = match ci {
                    "success" => k.c.success.fg,
                    "failure" => k.c.danger.fg,
                    _ => k.c.warning.fg,
                };
                top_row = top_row.child(el().size(6.0, 6.0).radius(3.0).fill(dot_color));
            }
            top_row = top_row
                .child(el().grow(1.0))
                .child(small_btn(
                    k,
                    t("Open"),
                    ButtonVariant::GhostMuted,
                    format!("pr-open-{asid}-{}", pr.number),
                    move |_| crate::platform::shell::open_url(&open_url),
                ))
                .child(small_btn(
                    k,
                    t("Checkout"),
                    ButtonVariant::Outline,
                    format!("pr-co-{asid}-{}", pr.number),
                    move |s| {
                        let mode = {
                            let gp = s.git_panel_mut(&checkout_asid);
                            gp.busy_action = Some("checkout".into());
                            gp.diff_mode.as_str().to_string()
                        };
                        s.send(Command::VcsCheckout {
                            asid: AgentSessionId(checkout_asid.clone()),
                            branch: None,
                            pr_number: Some(pr_num),
                            mode,
                        });
                    },
                ));

            let mut meta_parts = Vec::new();
            if !pr.head_branch.is_empty() && !pr.base_branch.is_empty() {
                meta_parts.push(format!("{} → {}", pr.head_branch, pr.base_branch));
            }
            if !pr.author.is_empty() {
                meta_parts.push(format!("@{}", pr.author));
            }
            let card = sub_card(k, format!("pr-{asid}-{}", pr.number))
                .child(top_row)
                .child(
                    text(pr.title.clone())
                        .font(k.ty.small_medium)
                        .color(k.c.text_base)
                        .truncate(),
                );
            let card = if !meta_parts.is_empty() {
                card.child(
                    text(meta_parts.join(" · "))
                        .font(k.ty.small)
                        .color(k.c.text_faint)
                        .truncate(),
                )
            } else {
                card
            };
            body.push(card);
        }

        if gp.pr_has_more {
            let next_page = gp.pr_page.saturating_add(1).max(2);
            let more_asid = asid.to_string();
            let label = if gp.pr_loading {
                t("Loading…")
            } else {
                t("Load more PRs")
            };
            body.push(
                Button::new(label)
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Regular)
                    .disabled(gp.pr_loading)
                    .on_press(move |s: &mut UiState| {
                        s.request_git_prs(&more_asid, next_page, false);
                    })
                    .build(k)
                    .key(format!("git-prs-load-more-{asid}"))
                    .w_full(),
            );
        }
    }

    let badge = (!gp.prs.is_empty()).then(|| {
        if gp.pr_has_more {
            format!("{}+", gp.prs.len())
        } else {
            gp.prs.len().to_string()
        }
    });
    section_card(
        k,
        format!("git-prs-card-{asid}"),
        &icons::FORK,
        t("Pull Requests"),
        badge,
        trailing,
        body,
    )
}

fn commits_card(s: &UiState, k: &Kit, asid: &str) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let gp = s.git_panel_for(asid);
    let open = gp.commits_open;
    let toggle_asid = asid.to_string();
    let toggle_btn = small_btn(
        k,
        if open { t("Hide") } else { t("Show") },
        ButtonVariant::GhostMuted,
        format!("git-commits-toggle-{asid}"),
        move |s| {
            let should_load = {
                let gp = s.git_panel_mut(&toggle_asid);
                gp.commits_open = !gp.commits_open;
                gp.commits_open && gp.commits.is_empty() && !gp.commits_loading
            };
            if should_load {
                s.request_git_commits(&toggle_asid, 0);
            }
        },
    );

    let mut body = Vec::new();
    if open {
        if gp.commits_loading && gp.commits.is_empty() {
            body.push(
                text(t("Loading commits…"))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            );
        } else if gp.commits.is_empty() {
            body.push(
                text(t("No commits yet"))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            );
        } else {
            for c in &gp.commits {
                let rel = format_relative_time(c.timestamp_ms);
                let meta = if rel.is_empty() {
                    c.author.clone()
                } else {
                    format!("{} · {rel}", c.author)
                };
                body.push(
                    sub_card(k, format!("commit-{asid}-{}", c.hash))
                        .child(
                            row()
                                .w_full()
                                .gap(6.0)
                                .center_y()
                                .child(
                                    text(c.short_hash.clone())
                                        .font(k.ty.mono)
                                        .color(k.c.text_accent),
                                )
                                .child(
                                    text(c.subject.clone())
                                        .font(k.ty.small_medium)
                                        .color(k.c.text_base)
                                        .truncate()
                                        .grow(1.0)
                                        .shrink()
                                        .min_w(0.0),
                                ),
                        )
                        .child(text(meta).font(k.ty.small).color(k.c.text_faint).truncate()),
                );
            }
            if gp.commits_has_more {
                let next_offset = gp.commits_offset;
                let more_asid = asid.to_string();
                body.push(
                    Button::new(if gp.commits_loading {
                        t("Loading…")
                    } else {
                        t("Load more commits")
                    })
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .disabled(gp.commits_loading)
                    .on_press(move |s: &mut UiState| {
                        s.request_git_commits(&more_asid, next_offset);
                    })
                    .build(k)
                    .key(format!("git-commits-more-{asid}"))
                    .w_full(),
                );
            }
        }
    }

    section_card(
        k,
        format!("git-commits-card-{asid}"),
        &parts::glyphs::TIME,
        t("Recent Commits"),
        (!gp.commits.is_empty()).then(|| gp.commits.len().to_string()),
        vec![toggle_btn],
        body,
    )
}

fn worktrees_card(
    s: &UiState,
    k: &Kit,
    asid: &str,
    worktrees: &[WorktreeRow],
    agent: &str,
    dir: &str,
) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let open = s.git_panel_for(asid).worktrees_open;
    let toggle_asid = asid.to_string();
    let toggle_btn = small_btn(
        k,
        if open { t("Hide") } else { t("Show") },
        ButtonVariant::GhostMuted,
        format!("git-wt-toggle-{asid}"),
        move |s| {
            let gp = s.git_panel_mut(&toggle_asid);
            gp.worktrees_open = !gp.worktrees_open;
        },
    );

    let mut body = Vec::new();
    if open {
        if worktrees.is_empty() {
            body.push(
                text(t("No linked worktrees"))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            );
        } else {
            for w in worktrees {
                let dir_path = PathBuf::from(&w.directory);
                let mut actions = row().gap(4.0).center_y().child(small_btn(
                    k,
                    t("Terminal"),
                    ButtonVariant::Outline,
                    format!("term-wt-{asid}-{}", w.directory),
                    move |s| {
                        s.term_new_tab_in(dir_path.clone(), None);
                    },
                ));
                if !w.is_root() && !agent.is_empty() && !dir.is_empty() {
                    let (a, project, worktree) =
                        (agent.to_string(), dir.to_string(), w.directory.clone());
                    let confirm_key = format!("rm-wt:{}", w.directory);
                    actions = actions.child(confirm_btn(
                        s,
                        k,
                        asid,
                        confirm_key,
                        (t("Remove"), t("Confirm")),
                        ButtonVariant::GhostMuted,
                        format!("rmw-{asid}-{}", w.directory),
                        move |s| {
                            s.worktrees.retain(|x| x.directory != worktree);
                            s.send(Command::RemoveWorktree {
                                agent: a.clone(),
                                project: project.clone(),
                                worktree: worktree.clone(),
                            });
                        },
                    ));
                }
                let strategy = w
                    .strategy
                    .clone()
                    .filter(|st| !st.is_empty())
                    .unwrap_or_else(|| tr(language, "root"));
                body.push(
                    sub_card(k, format!("wt-{asid}-{}", w.directory))
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
                        ),
                );
            }
        }
    }

    let linked_count = worktrees.iter().filter(|w| !w.is_root()).count();
    section_card(
        k,
        format!("git-worktrees-card-{asid}"),
        &icons::BRANCH,
        t("Worktrees"),
        (linked_count > 0).then(|| linked_count.to_string()),
        vec![toggle_btn],
        body,
    )
}

/// Render the Git Workbench for a specific session `asid`.
/// When `inline == true`, renders without the Target Session Bar (used inside a tile drawer).
pub(crate) fn view_for_session(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    asid: &str,
    inline: bool,
) -> El {
    let language = s.cfg.general.language;
    let t = |en: &'static str| tr_ref(language, en);
    let info = (!asid.is_empty()).then(|| s.session_by_id(asid)).flatten();
    let cwd = info.and_then(|i| i.directory.clone());
    let agent_id = info.map(|i| i.agent_id.clone()).unwrap_or_default();
    let comp = (!asid.is_empty())
        .then(|| s.computer_of_session(asid))
        .flatten()
        .unwrap_or_else(|| s.current_computer().to_string());
    let is_remote = UiState::is_remote_computer_id(&comp);
    let gp = s.git_panel_for(asid);

    let local_branch_fallback = (!is_remote)
        .then(|| cwd.as_ref().and_then(|c| s.branches.get(c)))
        .flatten();

    let is_repo = match gp.overview.as_ref() {
        Some(ov) => ov.is_repo,
        None => {
            if gp.files.is_repo && !gp.files.files.is_empty() {
                true
            } else {
                local_branch_fallback.is_some_and(|b| b.current.is_some())
            }
        }
    };

    let mut cards = Vec::new();
    let refresh_asid = asid.to_string();
    let refresh_btn = IconButton::new(&icons::REFRESH)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .label(t("Refresh"))
        .help(t("Refresh"))
        .on_press(move |s: &mut UiState| s.refresh_git_panel_for(&refresh_asid, true))
        .build(k)
        .key(format!("git-refresh-btn-{asid}"));

    if asid.is_empty() {
        cards.push(section_card(
            k,
            "git-no-session-card",
            &icons::BRANCH,
            "Git",
            None,
            Vec::new(),
            vec![
                text(t("No session selected"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_muted),
                text(t(
                    "Open a session to inspect its branch, diffs, and pull requests.",
                ))
                .font(k.ty.small)
                .color(k.c.text_faint),
            ],
        ));
    } else if !is_repo && !gp.overview_loading {
        let cwd_path = cwd.as_ref().map(PathBuf::from);
        let init_btn = small_btn(
            k,
            t("Initialize Git in Terminal"),
            ButtonVariant::Outline,
            format!("git-init-term-{asid}"),
            move |s| {
                let dir = cwd_path.clone().unwrap_or_else(|| s.terminal_cwd());
                s.term_new_tab_in(dir, Some("git init && git status\n".into()));
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
            text(t("This folder is not a git checkout."))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
        if let Some(err) = gp.overview_error.as_deref().filter(|_| is_remote) {
            body.push(text(err.to_string()).font(k.ty.small).color(k.c.text_faint));
        }
        body.push(row().pt(2.0).child(init_btn));
        cards.push(section_card(
            k,
            format!("git-no-repo-card-{asid}"),
            &icons::BRANCH,
            t("No repository"),
            None,
            vec![refresh_btn],
            body,
        ));
    } else {
        // ---- 1. Repository & Sync Card ----
        let cur_branch = gp
            .overview
            .as_ref()
            .and_then(|o| o.branch.clone())
            .or_else(|| local_branch_fallback.and_then(|b| b.current.clone()))
            .unwrap_or_else(|| "HEAD".into());
        let local_branches: Vec<String> = gp
            .overview
            .as_ref()
            .map(|o| o.local_branches.clone())
            .filter(|v| !v.is_empty())
            .or_else(|| local_branch_fallback.map(|b| b.local.clone()))
            .unwrap_or_default();
        let remote_url = gp
            .overview
            .as_ref()
            .and_then(|o| o.remote_url.clone())
            .or_else(|| local_branch_fallback.and_then(|b| b.remote_url.clone()));
        let ahead = gp.overview.as_ref().map_or(0, |o| o.ahead);
        let behind = gp.overview.as_ref().map_or(0, |o| o.behind);
        let has_gateway_ops = gp.overview.is_some();

        let cwd_path = cwd.as_ref().map(PathBuf::from);
        let open_term = small_btn(
            k,
            t("Terminal"),
            ButtonVariant::Outline,
            format!("git-open-term-{asid}"),
            move |s| {
                let dir = cwd_path.clone().unwrap_or_else(|| s.terminal_cwd());
                s.term_new_tab_in(dir, Some("git status\n".into()));
            },
        );

        let mut repo_body = vec![branch_switcher_dropdown(
            s,
            k,
            layer,
            asid,
            &cur_branch,
            &local_branches,
            has_gateway_ops,
            cwd.as_deref(),
        )];

        if ahead > 0 || behind > 0 || gp.overview.as_ref().is_some_and(|o| o.upstream.is_some()) {
            let mut sync_info = row().w_full().gap(6.0).center_y();
            if ahead > 0 {
                sync_info = sync_info.child(
                    Badge::new(format!("↑{ahead}"))
                        .variant(BadgeVariant::Accent)
                        .build(k),
                );
            }
            if behind > 0 {
                sync_info = sync_info.child(
                    Badge::new(format!("↓{behind}"))
                        .variant(BadgeVariant::Subtle)
                        .build(k),
                );
            }
            if let Some(up) = gp.overview.as_ref().and_then(|o| o.upstream.as_deref()) {
                sync_info = sync_info.child(
                    text(up.to_string())
                        .font(k.ty.small)
                        .color(k.c.text_faint)
                        .truncate(),
                );
            }
            repo_body.push(sync_info);
        }

        let mut action_row = row().w_full().gap(6.0).center_y().flex_wrap();
        if has_gateway_ops {
            let busy = gp.busy_action.is_some();
            for (action_id, label) in [
                ("fetch", t("Fetch")),
                ("pull", t("Pull")),
                ("push", t("Push")),
            ] {
                let a_asid = asid.to_string();
                let act = action_id.to_string();
                action_row = action_row.child(
                    Button::new(label)
                        .variant(if action_id == "push" && ahead > 0 {
                            ButtonVariant::Neutral
                        } else {
                            ButtonVariant::Outline
                        })
                        .control_size(ControlSize::Small)
                        .disabled(busy)
                        .on_press(move |s: &mut UiState| {
                            let mode = {
                                let gp = s.git_panel_mut(&a_asid);
                                gp.busy_action = Some(act.clone());
                                gp.diff_mode.as_str().to_string()
                            };
                            s.send(Command::VcsSync {
                                asid: AgentSessionId(a_asid.clone()),
                                action: act.clone(),
                                mode,
                            });
                        })
                        .build(k)
                        .key(format!("git-sync-{asid}-{action_id}")),
                );
            }
        }
        if let Some(url) = remote_url.as_deref() {
            let repo_view_url = url.to_string();
            action_row = action_row.child(small_btn(
                k,
                t("Repo"),
                ButtonVariant::GhostMuted,
                format!("git-view-repo-{asid}"),
                move |_| crate::platform::shell::open_url(&repo_view_url),
            ));
        }
        repo_body.push(action_row);

        if let Some(err) = gp.overview_error.as_deref() {
            repo_body.push(text(err.to_string()).font(k.ty.small).color(k.c.text_faint));
        }

        cards.push(section_card(
            k,
            format!("git-branch-card-{asid}"),
            &icons::BRANCH,
            t("Repository"),
            (!local_branches.is_empty()).then(|| local_branches.len().to_string()),
            vec![open_term, refresh_btn],
            repo_body,
        ));

        // ---- 2. Changes Card ----
        let mode_asid_w = asid.to_string();
        let mode_asid_b = asid.to_string();
        let mode_toggle = row()
            .gap(4.0)
            .center_y()
            .child(filter_pill(
                k,
                t("Working"),
                gp.diff_mode == GitDiffMode::Working,
                format!("git-mode-working-{asid}"),
                move |s| {
                    if s.git_panel_for(&mode_asid_w).diff_mode != GitDiffMode::Working {
                        let gp = s.git_panel_mut(&mode_asid_w);
                        gp.diff_mode = GitDiffMode::Working;
                        gp.files_limit = INITIAL_FILES_LIMIT;
                        s.send(Command::VcsLoadOverview {
                            asid: AgentSessionId(mode_asid_w.clone()),
                            mode: "working".into(),
                        });
                    }
                },
            ))
            .child(filter_pill(
                k,
                t("Branch"),
                gp.diff_mode == GitDiffMode::Branch,
                format!("git-mode-branch-{asid}"),
                move |s| {
                    if s.git_panel_for(&mode_asid_b).diff_mode != GitDiffMode::Branch {
                        let gp = s.git_panel_mut(&mode_asid_b);
                        gp.diff_mode = GitDiffMode::Branch;
                        gp.files_limit = INITIAL_FILES_LIMIT;
                        s.send(Command::VcsLoadOverview {
                            asid: AgentSessionId(mode_asid_b.clone()),
                            mode: "branch".into(),
                        });
                    }
                },
            ));

        let files_list: Vec<GitChangedFile> = if !gp.files.files.is_empty() || gp.overview.is_some()
        {
            gp.files.files.clone()
        } else if s.selected.as_deref() == Some(asid) {
            s.diff_stat
                .paths
                .iter()
                .map(|p| GitChangedFile {
                    path: p.path.clone(),
                    old_path: None,
                    status: "modified".into(),
                    staged: false,
                    unstaged: true,
                    binary: false,
                    added: Some(p.added as u64),
                    removed: Some(p.removed as u64),
                })
                .collect()
        } else {
            Vec::new()
        };

        let mut changes_body = vec![mode_toggle];
        let total_files = files_list.len();

        if total_files == 0 {
            changes_body.push(
                text(if gp.diff_mode == GitDiffMode::Working {
                    t("Working tree is clean")
                } else {
                    t("No diff against base branch")
                })
                .font(k.ty.small)
                .color(k.c.text_faint),
            );
        } else {
            let staged_files: Vec<&GitChangedFile> = if gp.diff_mode == GitDiffMode::Working {
                files_list.iter().filter(|f| f.staged).collect()
            } else {
                Vec::new()
            };
            let unstaged_files: Vec<&GitChangedFile> = if gp.diff_mode == GitDiffMode::Working {
                files_list
                    .iter()
                    .filter(|f| f.unstaged || !f.staged)
                    .collect()
            } else {
                files_list.iter().collect()
            };

            let mut remaining = gp.files_limit;

            if !staged_files.is_empty() {
                let unstage_asid = asid.to_string();
                let mut hdr = row()
                    .w_full()
                    .center_y()
                    .gap(6.0)
                    .child(
                        text(format!("{} ({})", t("Staged"), staged_files.len()))
                            .font(k.ty.small_medium)
                            .color(k.c.success.fg),
                    )
                    .child(el().grow(1.0));
                if has_gateway_ops {
                    hdr = hdr.child(small_btn(
                        k,
                        t("Unstage all"),
                        ButtonVariant::GhostMuted,
                        format!("git-unstage-all-{asid}"),
                        move |s| {
                            let mode = s
                                .git_panel_for(&unstage_asid)
                                .diff_mode
                                .as_str()
                                .to_string();
                            s.send(Command::VcsUnstage {
                                asid: AgentSessionId(unstage_asid.clone()),
                                paths: Vec::new(),
                                all: true,
                                mode,
                            });
                        },
                    ));
                }
                changes_body.push(hdr);
                for (i, f) in staged_files.iter().take(remaining).enumerate() {
                    changes_body.push(file_row_view(
                        s,
                        k,
                        asid,
                        f,
                        i,
                        true,
                        has_gateway_ops,
                        false,
                    ));
                }
                remaining = remaining.saturating_sub(staged_files.len());
            }

            if !unstaged_files.is_empty() && remaining > 0 {
                let stage_asid = asid.to_string();
                let group_title = if gp.diff_mode == GitDiffMode::Working {
                    format!("{} ({})", t("Changes"), unstaged_files.len())
                } else {
                    format!("{} ({})", t("Branch diff"), unstaged_files.len())
                };
                let mut hdr = row()
                    .w_full()
                    .center_y()
                    .gap(6.0)
                    .child(
                        text(group_title)
                            .font(k.ty.small_medium)
                            .color(k.c.text_muted),
                    )
                    .child(el().grow(1.0));
                if has_gateway_ops && gp.diff_mode == GitDiffMode::Working {
                    hdr = hdr.child(small_btn(
                        k,
                        t("Stage all"),
                        ButtonVariant::GhostMuted,
                        format!("git-stage-all-{asid}"),
                        move |s| {
                            let mode = s.git_panel_for(&stage_asid).diff_mode.as_str().to_string();
                            s.send(Command::VcsStage {
                                asid: AgentSessionId(stage_asid.clone()),
                                paths: Vec::new(),
                                all: true,
                                mode,
                            });
                        },
                    ));
                }
                changes_body.push(hdr);
                for (i, f) in unstaged_files.iter().take(remaining).enumerate() {
                    changes_body.push(file_row_view(
                        s,
                        k,
                        asid,
                        f,
                        i,
                        false,
                        has_gateway_ops && gp.diff_mode == GitDiffMode::Working,
                        gp.diff_mode == GitDiffMode::Working,
                    ));
                }
            }

            let shown_total = staged_files.len() + unstaged_files.len();
            if shown_total > gp.files_limit {
                let more_count = shown_total - gp.files_limit;
                let more_asid = asid.to_string();
                let all_asid = asid.to_string();
                changes_body.push(
                    row()
                        .w_full()
                        .gap(6.0)
                        .child(
                            small_btn(
                                k,
                                &format!("{} ({more_count})", t("Show more")),
                                ButtonVariant::Outline,
                                format!("git-files-more-{asid}"),
                                move |s| s.git_panel_mut(&more_asid).files_limit += FILES_PAGE_STEP,
                            )
                            .grow(1.0),
                        )
                        .child(small_btn(
                            k,
                            t("Show all"),
                            ButtonVariant::GhostMuted,
                            format!("git-files-all-{asid}"),
                            move |s| s.git_panel_mut(&all_asid).files_limit = shown_total,
                        )),
                );
            }

            if gp.diff_mode == GitDiffMode::Working && (has_gateway_ops || !is_remote) {
                let stage_all = staged_files.is_empty();
                let commit_label = if stage_all {
                    format!("{} ({})", t("Commit All"), total_files)
                } else {
                    format!("{} ({})", t("Commit Staged"), staged_files.len())
                };
                let has_msg = !s.git_commit_msg.text.trim().is_empty();
                let busy = gp.busy_action.is_some();
                let sub_asid = asid.to_string();
                let btn_asid = asid.to_string();
                let push_asid = asid.to_string();
                let commit_input = TextInput::new(&s.git_commit_msg)
                    .placeholder(t("Commit message…"))
                    .label(t("Commit message"))
                    .on_edit(|s: &mut UiState| &mut s.git_commit_msg)
                    .on_submit(move |s: &mut UiState, _: &mut Tx| {
                        commit_changes(s, &sub_asid, false, stage_all)
                    })
                    .build(k)
                    .key(format!("git-commit-msg-field-{asid}"))
                    .w_full();
                changes_body.push(
                    col()
                        .key(format!("git-commit-box-{asid}"))
                        .w_full()
                        .pt(4.0)
                        .gap(6.0)
                        .child(el().w_full().h(1.0).fill(k.c.border_muted).a11y_hidden())
                        .child(commit_input)
                        .child(
                            row()
                                .w_full()
                                .gap(6.0)
                                .center_y()
                                .child(
                                    Button::new(commit_label)
                                        .variant(ButtonVariant::Neutral)
                                        .control_size(ControlSize::Regular)
                                        .disabled(!has_msg || busy)
                                        .on_press(move |s: &mut UiState| {
                                            commit_changes(s, &btn_asid, false, stage_all)
                                        })
                                        .build(k)
                                        .key(format!("git-commit-btn-{asid}")),
                                )
                                .child(
                                    Button::new(t("Commit & Push"))
                                        .variant(ButtonVariant::Submit)
                                        .control_size(ControlSize::Regular)
                                        .disabled(!has_msg || busy)
                                        .on_press(move |s: &mut UiState| {
                                            commit_changes(s, &push_asid, true, stage_all)
                                        })
                                        .build(k)
                                        .key(format!("git-commit-push-btn-{asid}")),
                                ),
                        ),
                );
            }
        }

        let diff_asid = asid.to_string();
        let view_diff_btn = (total_files > 0).then(|| {
            small_btn(
                k,
                t("View diff"),
                ButtonVariant::Outline,
                format!("view-changes-{asid}"),
                move |s| {
                    if s.selected.as_deref() != Some(diff_asid.as_str()) {
                        if let Some(tile_idx) = s.tile_of(&diff_asid) {
                            s.focus_tile(tile_idx);
                        }
                    }
                    s.mode = Mode::Session;
                    s.show_diff = true;
                },
            )
        });
        cards.push(section_card(
            k,
            format!("git-changes-card-{asid}"),
            &parts::glyphs::DIFF,
            t("Changes"),
            (total_files > 0).then(|| total_files.to_string()),
            view_diff_btn.into_iter().collect(),
            changes_body,
        ));

        // ---- 3. Pull Requests Card (Paginated) ----
        cards.push(pull_requests_card(s, k, asid, remote_url.as_deref()));

        // ---- 4. Collapsible Recent Commits Card ----
        if has_gateway_ops {
            cards.push(commits_card(s, k, asid));
        }

        // ---- 5. Collapsible Universal Worktrees Card ----
        let worktrees = gp
            .overview
            .as_ref()
            .filter(|o| !o.worktrees.is_empty())
            .map(|o| o.worktrees.as_slice())
            .unwrap_or(&s.worktrees);
        cards.push(worktrees_card(
            s,
            k,
            asid,
            worktrees,
            &agent_id,
            cwd.as_deref().unwrap_or(""),
        ));
    }

    let scroll_asid = asid.to_string();
    let scroll = col()
        .key(format!("git-panel-scroll-{asid}"))
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
        .on_at_bottom(move |s: &mut UiState, at| {
            if !at || scroll_asid.is_empty() {
                return;
            }
            let next_page = {
                let gp = s.git_panel_for(&scroll_asid);
                if !gp.pr_has_more || gp.pr_loading || gp.prs.is_empty() {
                    return;
                }
                if gp
                    .pr_last_auto_load
                    .is_some_and(|t| t.elapsed() < PR_AUTO_LOAD_COOLDOWN)
                {
                    return;
                }
                gp.pr_page.saturating_add(1).max(2)
            };
            s.git_panel_mut(&scroll_asid).pr_last_auto_load = Some(Instant::now());
            s.request_git_prs(&scroll_asid, next_page, false);
        })
        .scroll_bar(k);

    let mut root = col()
        .key(if inline {
            format!("git-panel-inline-{asid}")
        } else {
            "git-panel".to_string()
        })
        .w_full()
        .h_full()
        .min_h(0.0);
    if !inline {
        root = root.child(target_session_bar(s, k, layer, "git"));
    }
    root.child(scroll)
}

/// Render the standalone top-level Git Workbench plugin panel.
pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let asid = s.panel_target_asid().unwrap_or_default().to_string();
    view_for_session(s, k, layer, &asid, false)
}

pub(crate) const COMMITS_LIMIT: usize = COMMITS_PAGE_SIZE;
pub(crate) const PRS_LIMIT: u32 = PRS_PAGE_SIZE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_pagination_appends_deduped_pages_and_resets_on_filter_mismatch() {
        let mut gp = GitPanelState {
            target_asid: Some("ses-1".into()),
            pr_state: "open",
            pr_author: "all",
            pr_loading: true,
            ..Default::default()
        };
        gp.apply_prs(
            "ses-1",
            "open",
            "all",
            false,
            Ok(GitPrsPage {
                host: "github".into(),
                supported: true,
                reason: None,
                repo_url: Some("https://github.com/org/repo".into()),
                items: vec![GitPrRow {
                    number: 10,
                    title: "First PR".into(),
                    state: "open".into(),
                    draft: false,
                    url: "https://github.com/org/repo/pull/10".into(),
                    author: "alice".into(),
                    head_branch: "feat-a".into(),
                    base_branch: "main".into(),
                    updated_at: String::new(),
                    checks_status: Some("success".into()),
                }],
                page: 1,
                next_page: Some(2),
                has_more: true,
            }),
        );
        assert_eq!(gp.prs.len(), 1);
        assert!(gp.pr_has_more);
        assert_eq!(gp.pr_page, 1);

        gp.apply_prs(
            "ses-1",
            "open",
            "all",
            true,
            Ok(GitPrsPage {
                host: "github".into(),
                supported: true,
                reason: None,
                repo_url: Some("https://github.com/org/repo".into()),
                items: vec![
                    GitPrRow {
                        number: 10,
                        title: "First PR".into(),
                        state: "open".into(),
                        draft: false,
                        url: "https://github.com/org/repo/pull/10".into(),
                        author: "alice".into(),
                        head_branch: "feat-a".into(),
                        base_branch: "main".into(),
                        updated_at: String::new(),
                        checks_status: None,
                    },
                    GitPrRow {
                        number: 9,
                        title: "Second PR".into(),
                        state: "open".into(),
                        draft: true,
                        url: "https://github.com/org/repo/pull/9".into(),
                        author: "bob".into(),
                        head_branch: "feat-b".into(),
                        base_branch: "main".into(),
                        updated_at: String::new(),
                        checks_status: None,
                    },
                ],
                page: 2,
                next_page: None,
                has_more: false,
            }),
        );
        assert_eq!(gp.prs.len(), 2);
        assert!(!gp.pr_has_more);
        assert_eq!(gp.pr_page, 2);

        gp.pr_author = "me";
        gp.apply_prs("ses-1", "open", "all", false, Ok(GitPrsPage::default()));
        assert_eq!(gp.prs.len(), 2);
    }
}
