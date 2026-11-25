//! Hierarchical session tree (App `agent-session-tree-sheet.tsx` parity):
//! renders the full Root → Parent → Siblings → Children / Forks hierarchy for
//! the active session, even when viewing a nested child session.
//!
//! Wired into the inspector / drawer surface by the window-system integration.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use muqun_gateway::agents::{AgentSessionInfo, AgentSessionStatus, TerminalRuntimeStatus};
use rune::{col, el, row, text, Color, El};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit,
};

use super::copy::tr_ref;
use super::{muted, title_of, UiState, MAX_TILES};

/// Visual indentation per tree depth level (px) and max visual nesting depth.
const INDENT_PX: f32 = 16.0;
const MAX_VISUAL_DEPTH: usize = 5;

/// One flattened node in a session's hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub asid: String,
    pub parent_id: Option<String>,
    pub agent_id: String,
    pub title: String,
    pub status: AgentSessionStatus,
    pub runtime_status: Option<TerminalRuntimeStatus>,
    pub role_or_mode: Option<String>,
    pub depth: usize,
    pub is_current: bool,
    pub is_last_sibling: bool,
    /// For each ancestor column `0..depth`, `true` if that ancestor still has
    /// following siblings (draw a vertical continuation guide).
    pub ancestor_continuations: Vec<bool>,
    pub is_fork: bool,
    pub has_children: bool,
}

fn clean_parent_ref<'a>(raw: Option<&'a str>, self_asid: &str) -> Option<&'a str> {
    raw.map(str::trim)
        .filter(|id| !id.is_empty() && *id != "null" && *id != self_asid)
}

fn effective_parent<'a>(
    info: &'a AgentSessionInfo,
    index: &HashMap<&str, &'a AgentSessionInfo>,
) -> (Option<&'a str>, bool) {
    if let Some(p) = clean_parent_ref(info.parent_id.as_deref(), &info.asid.0) {
        return (Some(p), false);
    }
    if let Some(fork_from) = info
        .fork
        .as_ref()
        .and_then(|f| clean_parent_ref(Some(f.session_id.as_str()), &info.asid.0))
    {
        if index.contains_key(fork_from) {
            return (Some(fork_from), true);
        }
    }
    (None, false)
}

fn format_role_or_mode(info: &AgentSessionInfo, is_fork: bool) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(name) = info
        .agent_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(name);
    }
    if let Some(role) = info
        .agent_role
        .as_deref()
        .or(info.mode.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !parts.iter().any(|p| p.eq_ignore_ascii_case(role)) {
            parts.push(role);
        }
    }
    if parts.is_empty() && is_fork {
        return Some("fork".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn is_busy(status: AgentSessionStatus) -> bool {
    matches!(status, AgentSessionStatus::Busy | AgentSessionStatus::Retry)
}

/// Pure, cycle-safe session tree builder.
///
/// Resolves the true root from `root_info` / `current_asid`, attaches nested
/// subagents and forked sessions, orders busy siblings first, and returns a
/// preorder list of [`TreeNode`]s ready for rendering.
pub fn build_tree<'a>(
    root_info: Option<&'a AgentSessionInfo>,
    descendants: &'a [AgentSessionInfo],
    sessions: &'a [AgentSessionInfo],
    current_asid: Option<&str>,
) -> Vec<TreeNode> {
    let mut index: HashMap<&str, &AgentSessionInfo> = HashMap::new();
    for info in sessions {
        if !info.deleted {
            index.insert(info.asid.0.as_str(), info);
        }
    }
    for info in descendants {
        if !info.deleted {
            index.insert(info.asid.0.as_str(), info);
        }
    }
    if let Some(info) = root_info {
        if !info.deleted {
            index.insert(info.asid.0.as_str(), info);
        }
    }

    let Some(start_asid) = root_info
        .map(|r| r.asid.0.as_str())
        .or(current_asid)
        .filter(|id| !id.trim().is_empty())
    else {
        return Vec::new();
    };

    // Walk up ancestor links (parent_id or fork.session_id) with cycle protection
    // so starting from any nested child still roots the tree at the top ancestor.
    let mut root_id = start_asid;
    let mut up_seen = HashSet::new();
    while up_seen.insert(root_id) {
        let Some(info) = index.get(root_id).copied() else {
            break;
        };
        let (parent, _) = effective_parent(info, &index);
        match parent {
            Some(p) if index.contains_key(p) && !up_seen.contains(p) => {
                root_id = p;
            }
            _ => break,
        }
    }

    let Some(&root) = index.get(root_id) else {
        return Vec::new();
    };

    // Group children by parent, tracking whether each link is a fork.
    let mut children_by_parent: HashMap<&str, Vec<(&AgentSessionInfo, bool)>> = HashMap::new();
    let mut seen_edges: HashSet<(&str, &str)> = HashSet::new();
    let mut add_child = |parent: &'a str, child: &'a AgentSessionInfo, is_fork: bool| {
        if child.deleted || child.asid.0 == parent {
            return;
        }
        if seen_edges.insert((parent, child.asid.0.as_str())) {
            children_by_parent
                .entry(parent)
                .or_default()
                .push((child, is_fork));
        }
    };

    // 1. Add all explicitly linked children from `descendants` first (preserving server order).
    let mut descendant_ids: HashSet<&str> = HashSet::new();
    for info in descendants {
        if info.deleted || info.asid.0 == root.asid.0 {
            continue;
        }
        descendant_ids.insert(info.asid.0.as_str());
        let (parent, is_fork) = effective_parent(info, &index);
        match parent {
            Some(p) if index.contains_key(p) => add_child(p, info, is_fork),
            _ => add_child(root.asid.0.as_str(), info, is_fork),
        }
    }

    // 2. Also include any children or forks present in `sessions` that weren't in `descendants`.
    for info in sessions {
        if info.deleted
            || info.asid.0 == root.asid.0
            || descendant_ids.contains(info.asid.0.as_str())
        {
            continue;
        }
        let (parent, is_fork) = effective_parent(info, &index);
        if let Some(p) = parent {
            if index.contains_key(p) {
                add_child(p, info, is_fork);
            }
        }
    }

    // Active/busy children first within each parent, preserving order within each bucket.
    for siblings in children_by_parent.values_mut() {
        siblings.sort_by_key(|(info, _)| !is_busy(info.status));
    }

    // Iterative preorder traversal with cycle protection.
    struct Frame<'a> {
        info: &'a AgentSessionInfo,
        parent_id: Option<&'a str>,
        depth: usize,
        is_last_sibling: bool,
        ancestor_continuations: Vec<bool>,
        is_fork: bool,
    }

    let mut out = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut stack = vec![Frame {
        info: root,
        parent_id: None,
        depth: 0,
        is_last_sibling: true,
        ancestor_continuations: Vec::new(),
        is_fork: false,
    }];

    while let Some(frame) = stack.pop() {
        let id = frame.info.as_id_str();
        if !visited.insert(id) {
            continue;
        }

        let children: Vec<(&AgentSessionInfo, bool)> = children_by_parent
            .get(id)
            .map(|list| {
                list.iter()
                    .copied()
                    .filter(|(c, _)| !visited.contains(c.as_id_str()))
                    .collect()
            })
            .unwrap_or_default();

        let has_children = !children.is_empty();
        let mut next_continuations = frame.ancestor_continuations.clone();
        if frame.depth > 0 {
            next_continuations.push(!frame.is_last_sibling);
        }

        out.push(TreeNode {
            asid: frame.info.asid.0.clone(),
            parent_id: frame.parent_id.map(str::to_owned),
            agent_id: frame.info.agent_id.clone(),
            title: title_of(frame.info),
            status: frame.info.status,
            runtime_status: frame.info.runtime.as_ref().and_then(|r| r.status),
            role_or_mode: format_role_or_mode(frame.info, frame.is_fork),
            depth: frame.depth,
            is_current: current_asid == Some(id),
            is_last_sibling: frame.is_last_sibling,
            ancestor_continuations: frame.ancestor_continuations,
            is_fork: frame.is_fork,
            has_children,
        });

        let count = children.len();
        for (i, (child, is_fork)) in children.into_iter().enumerate().rev() {
            stack.push(Frame {
                info: child,
                parent_id: Some(id),
                depth: frame.depth + 1,
                is_last_sibling: i + 1 == count,
                ancestor_continuations: next_continuations.clone(),
                is_fork,
            });
        }
    }

    out
}

trait SessionIdStr {
    fn as_id_str(&self) -> &str;
}

impl SessionIdStr for AgentSessionInfo {
    fn as_id_str(&self) -> &str {
        self.asid.0.as_str()
    }
}

fn status_badge(
    k: &Kit,
    language: crate::config::Language,
    status: AgentSessionStatus,
    runtime: Option<TerminalRuntimeStatus>,
) -> (&'static str, Color) {
    match runtime {
        Some(TerminalRuntimeStatus::Blocked) => (tr_ref(language, "Needs input"), k.c.warning.fg),
        Some(TerminalRuntimeStatus::Completed) => (tr_ref(language, "Completed"), k.c.success.fg),
        _ => match status {
            AgentSessionStatus::Busy => (tr_ref(language, "Running"), k.c.text_accent),
            AgentSessionStatus::Retry => (tr_ref(language, "Retrying"), k.c.warning.fg),
            AgentSessionStatus::Idle => (tr_ref(language, "Idle"), k.c.text_muted),
            AgentSessionStatus::Failed => (tr_ref(language, "Failed"), k.c.danger.fg),
            AgentSessionStatus::Interrupted => (tr_ref(language, "Stopped"), k.c.text_muted),
            AgentSessionStatus::Unknown => (tr_ref(language, "Unknown"), k.c.text_faint),
        },
    }
}

fn connector_guides(k: &Kit, node: &TreeNode) -> Option<El> {
    let visual_depth = node.depth.min(MAX_VISUAL_DEPTH);
    if visual_depth == 0 {
        return None;
    }
    let guide_color = k.c.border_muted;
    let mut guides = row()
        .key(format!("tree-guides-{}", node.asid))
        .h_full()
        .shrink()
        .a11y_hidden();

    // Vertical continuation rails for ancestor depths `0..(visual_depth - 1)`.
    for col_idx in 0..visual_depth.saturating_sub(1) {
        let continues = node
            .ancestor_continuations
            .get(col_idx)
            .copied()
            .unwrap_or(false);
        let mut cell = el().w(INDENT_PX).h_full();
        if continues {
            cell = cell.child(el().w(1.0).h_full().fill(guide_color).absolute(7.0, 0.0));
        }
        guides = guides.child(cell);
    }

    // Elbow connector (`├─` or `└─`) into this row.
    let mut elbow = el().w(INDENT_PX).h_full();
    if node.is_last_sibling {
        elbow = elbow.child(el().w(1.0).h(22.0).fill(guide_color).absolute(7.0, 0.0));
    } else {
        elbow = elbow.child(el().w(1.0).h_full().fill(guide_color).absolute(7.0, 0.0));
    }
    elbow = elbow.child(
        el().w(INDENT_PX - 7.0)
            .h(1.0)
            .fill(guide_color)
            .absolute(7.0, 22.0),
    );
    guides = guides.child(elbow);
    Some(guides)
}

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

fn tree_row(s: &UiState, k: &Kit, node: &TreeNode, root_asid: &str) -> El {
    let language = s.cfg.general.language;
    let (status_label, status_color) = status_badge(k, language, node.status, node.runtime_status);

    let in_tile = s.tile_of(&node.asid).is_some();
    let is_child = node.depth > 0;
    let parent_for_split = node
        .parent_id
        .clone()
        .unwrap_or_else(|| root_asid.to_owned());
    let parent_for_open = parent_for_split.clone();
    let split_asid = node.asid.clone();
    let open_asid = node.asid.clone();
    let split_agent = node.agent_id.clone();
    let open_agent = node.agent_id.clone();

    let split_btn = small_btn(
        k,
        if in_tile {
            tr_ref(language, "Tiled")
        } else {
            tr_ref(language, "Split")
        },
        if in_tile {
            ButtonVariant::Outline
        } else {
            ButtonVariant::Ghost
        },
        format!("tree-split-{}", node.asid),
        move |s| {
            if is_child {
                s.remember_subagent_context_with_agent(
                    &split_asid,
                    Some(&parent_for_split),
                    (!split_agent.is_empty()).then_some(split_agent.as_str()),
                );
            }
            s.open_in_tile(&split_asid);
        },
    );

    let mut actions = row().gap(4.0).center_y().child(split_btn);
    if !node.is_current {
        actions = actions.child(small_btn(
            k,
            tr_ref(language, "Open"),
            ButtonVariant::Ghost,
            format!("tree-open-{}", node.asid),
            move |s| {
                if is_child {
                    s.remember_subagent_context_with_agent(
                        &open_asid,
                        Some(&parent_for_open),
                        (!open_agent.is_empty()).then_some(open_agent.as_str()),
                    );
                }
                s.open_session(&open_asid);
            },
        ));
    }

    let mut meta = row()
        .gap(6.0)
        .center_y()
        .flex_wrap()
        .child(
            text(s.agent_name(&node.agent_id))
                .font(k.ty.small)
                .color(k.c.text_muted),
        )
        .child(
            text(status_label)
                .font(k.ty.small_medium)
                .color(status_color),
        );
    if let Some(role) = &node.role_or_mode {
        meta = meta.child(
            Badge::new(role.clone())
                .variant(BadgeVariant::Subtle)
                .build(k),
        );
    }
    if node.is_current {
        meta = meta.child(
            Badge::new(tr_ref(language, "Current"))
                .variant(BadgeVariant::Accent)
                .build(k),
        );
    }

    let copy = col()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(3.0)
        .child(
            row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(super::agents::mark(&node.agent_id, 13.0, s.dark).a11y_hidden())
                .child(
                    text(node.title.clone())
                        .font(if node.is_current {
                            k.ty.ui_medium
                        } else {
                            k.ty.ui
                        })
                        .color(k.c.text_base)
                        .truncate()
                        .shrink()
                        .grow(1.0)
                        .min_w(0.0),
                ),
        )
        .child(meta);

    let mut card = row()
        .key(format!("tree-card-{}", node.asid))
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(8.0)
        .center_y()
        .px(10.0)
        .py(7.0)
        .radius(k.radius.lg)
        .child(copy)
        .child(actions);

    if node.is_current {
        card = card
            .fill(k.c.bg_accent.alpha(if s.dark { 0.16 } else { 0.10 }))
            .border(1.0, k.c.bg_accent.alpha(0.45));
    } else {
        card = card
            .fill(k.c.layer_01.alpha(0.5))
            .border(1.0, k.c.border_muted);
    }

    let mut outer = row()
        .key(format!("tree-node-{}", node.asid))
        .w_full()
        .min_h(44.0)
        .center_y();
    if let Some(guides) = connector_guides(k, node) {
        outer = outer.child(guides);
    }
    outer.child(card)
}

/// Render the hierarchical session tree for the active session.
pub(crate) fn view(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let current_asid = s.selected.as_deref();
    let root_id = current_asid.map(|id| s.session_tree_root_id(id));
    let root_info = root_id
        .and_then(|id| s.session_by_id(id))
        .or_else(|| s.current());

    let nodes = build_tree(root_info, &s.descendants, &s.sessions, current_asid);
    if nodes.is_empty() {
        return vec![col()
            .key("empty-session-tree")
            .w_full()
            .gap(2.0)
            .px(4.0)
            .py(8.0)
            .child(
                text(tr_ref(language, "No sub-agents"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_muted),
            )
            .child(muted(
                k,
                tr_ref(language, "Sessions this one delegates to appear here."),
            ))];
    }

    let resolved_root = nodes[0].asid.clone();
    let mut out = Vec::with_capacity(nodes.len() + 2);

    let mut header = row()
        .key("session-tree-header")
        .w_full()
        .center_y()
        .gap(6.0)
        .px(4.0)
        .py(2.0)
        .child(
            text(tr_ref(language, "Session tree"))
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        )
        .child(
            Badge::new(nodes.len().to_string())
                .variant(BadgeVariant::Subtle)
                .build(k),
        )
        .child(el().grow(1.0))
        .child(
            IconButton::new(&super::icons::REFRESH)
                .variant(IconButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Refresh"))
                .help(tr_ref(language, "Refresh"))
                .on_press(|s: &mut UiState| s.refresh_session_tree())
                .build(k)
                .key("session-tree-refresh"),
        );

    if nodes.len() > 1 {
        let tile_targets: Vec<(String, Option<String>, String, bool)> = nodes
            .iter()
            .filter(|n| !n.is_current)
            .take(MAX_TILES.saturating_sub(1))
            .map(|n| {
                (
                    n.asid.clone(),
                    n.parent_id.clone().or_else(|| Some(resolved_root.clone())),
                    n.agent_id.clone(),
                    n.depth > 0,
                )
            })
            .collect();
        header = header.child(small_btn(
            k,
            tr_ref(language, "Tile all"),
            ButtonVariant::Outline,
            "tile-all-subagents".into(),
            move |s| {
                for (id, parent, agent, is_child) in &tile_targets {
                    if *is_child {
                        s.remember_subagent_context_with_agent(
                            id,
                            parent.as_deref(),
                            (!agent.is_empty()).then_some(agent.as_str()),
                        );
                    }
                    s.open_in_tile(id);
                }
            },
        ));
    }
    out.push(header);

    for node in &nodes {
        out.push(tree_row(s, k, node, &resolved_root));
    }

    if nodes.len() == 1 {
        out.push(
            col()
                .key("session-tree-no-children")
                .w_full()
                .px(4.0)
                .pt(4.0)
                .child(muted(k, tr_ref(language, "No sub-agents or forks yet."))),
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands::{self, Command};
    use crate::model::inbox::Update;
    use serde_json::json;

    fn mk_session(id: &str, agent: &str, parent: Option<&str>) -> AgentSessionInfo {
        serde_json::from_value(json!({
            "asid": id,
            "agent_id": agent,
            "title": id,
            "status": "idle",
            "parent_id": parent,
            "updated_ms": 100
        }))
        .unwrap()
    }

    #[test]
    fn build_tree_preserves_hierarchy_depths_sibling_flags_and_active_highlight() {
        let root = mk_session("root", "codex", None);
        let mut child_idle = mk_session("child-a", "codex", Some("root"));
        child_idle.agent_role = Some("explorer".into());
        let mut child_busy = mk_session("child-b", "claude", Some("root"));
        child_busy.status = AgentSessionStatus::Busy;
        child_busy.agent_name = Some("Reviewer".into());
        let grandchild = mk_session("grandchild", "codex", Some("child-b"));
        let mut forked = mk_session("fork-1", "codex", None);
        forked.fork = Some(muqun_gateway::agents::SessionForkInfo {
            session_id: "root".into(),
            boundary_type: None,
            message_id: None,
        });

        let descendants = vec![child_idle, child_busy, grandchild];
        let sessions = vec![root.clone(), forked];
        let nodes = build_tree(Some(&root), &descendants, &sessions, Some("grandchild"));

        let ids: Vec<&str> = nodes.iter().map(|n| n.asid.as_str()).collect();
        // Busy child-b comes first under root, followed by its grandchild, then child-a, then fork-1.
        assert_eq!(
            ids,
            vec!["root", "child-b", "grandchild", "child-a", "fork-1"]
        );

        assert_eq!(nodes[0].depth, 0);
        assert!(nodes[0].has_children);
        assert!(!nodes[0].is_current);

        assert_eq!(nodes[1].depth, 1);
        assert!(!nodes[1].is_last_sibling);
        assert_eq!(nodes[1].role_or_mode.as_deref(), Some("Reviewer"));

        assert_eq!(nodes[2].depth, 2);
        assert!(nodes[2].is_current);
        assert!(nodes[2].is_last_sibling);
        assert_eq!(nodes[2].ancestor_continuations, vec![true]);

        assert_eq!(nodes[3].depth, 1);
        assert!(!nodes[3].is_last_sibling);
        assert_eq!(nodes[3].role_or_mode.as_deref(), Some("explorer"));

        assert_eq!(nodes[4].depth, 1);
        assert!(nodes[4].is_last_sibling);
        assert!(nodes[4].is_fork);
        assert_eq!(nodes[4].role_or_mode.as_deref(), Some("fork"));
    }

    #[test]
    fn build_tree_resolves_root_when_invoked_from_nested_child() {
        let root = mk_session("root", "claude", None);
        let parent = mk_session("parent", "claude", Some("root"));
        let sibling = mk_session("sibling", "claude", Some("root"));
        let child = mk_session("child", "codex", Some("parent"));

        let descendants = vec![parent.clone(), sibling, child.clone()];
        let sessions = vec![root];
        // Calling with the child as `root_info` still walks up to `root`.
        let nodes = build_tree(Some(&child), &descendants, &sessions, Some("child"));
        let ids: Vec<&str> = nodes.iter().map(|n| n.asid.as_str()).collect();
        assert_eq!(ids, vec!["root", "parent", "child", "sibling"]);
        assert!(nodes.iter().find(|n| n.asid == "child").unwrap().is_current);
    }

    #[test]
    fn build_tree_handles_cycles_without_infinite_loop() {
        let mut a = mk_session("a", "codex", Some("b"));
        let b = mk_session("b", "codex", Some("a"));
        let self_loop = mk_session("c", "codex", Some("c"));
        a.title = "Cycle A".into();

        let nodes = build_tree(Some(&a), &[a.clone(), b.clone(), self_loop], &[], Some("a"));
        assert!(!nodes.is_empty());
        let unique: HashSet<&str> = nodes.iter().map(|n| n.asid.as_str()).collect();
        assert_eq!(unique.len(), nodes.len(), "no duplicate nodes on cycles");
    }

    #[test]
    fn refresh_session_tree_loads_root_descendants_when_child_is_selected() {
        let (tx, mut rx) = commands::channel();
        let mut s = UiState::new(tx);
        s.cfg.current_computer = "local".into();
        let root = mk_session("root", "claude", None);
        let child = mk_session("child", "codex", Some("root"));
        let sibling = mk_session("sibling", "claude", Some("root"));
        s.sessions = vec![root, child.clone(), sibling.clone()];
        s.selected = Some("child".into());

        s.refresh_session_tree();
        let sent: Vec<Command> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            sent.iter().any(|c| matches!(
                c,
                Command::LoadDescendants { asid } if asid.0 == "root"
            )),
            "expected LoadDescendants for root, got {sent:?}"
        );

        // Descendants arriving for `root` populate `s.descendants` even though `child` is selected.
        s.apply_updates(vec![Update::Descendants {
            asid: "root".into(),
            sessions: vec![child, sibling],
        }]);
        assert_eq!(s.descendants.len(), 2);

        let k = Kit::neutral(true);
        let els = view(&s, &k);
        assert!(els.len() >= 4, "header + root + 2 children");
    }
}
