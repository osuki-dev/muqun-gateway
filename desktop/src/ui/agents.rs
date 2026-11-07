//! How an agent is named and shown: standing words, brand marks, brand colour.

use muqun_gateway::platform::discovery::{AgentAvailability, AgentDiscoveryInfo};
use rune::{icon, Color, El, Icon};

use super::agent_marks;
use crate::config::Language;
use crate::hub::AGENT_PREFERENCE;
use crate::theme::agents as brand;

/// The six words (plus "checking" while a cold probe runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Usable,
    NeedsSetup,
    NotRunning,
    NotInstalled,
    NeedsUpdate,
    Disabled,
    Checking,
}

impl Standing {
    pub fn of(info: &AgentDiscoveryInfo) -> Self {
        Self::from_status(info.enabled, &info.status)
    }

    pub fn from_status(enabled: bool, status: &AgentAvailability) -> Self {
        if !enabled {
            return Self::Disabled;
        }
        match status {
            AgentAvailability::Connected | AgentAvailability::Reachable => Self::Usable,
            AgentAvailability::Unconfigured => Self::NeedsSetup,
            AgentAvailability::Offline => Self::NotRunning,
            AgentAvailability::NotInstalled => Self::NotInstalled,
            AgentAvailability::Unsupported => Self::NeedsUpdate,
            AgentAvailability::Disabled => Self::Disabled,
            AgentAvailability::Unknown => Self::Checking,
        }
    }

    pub fn word(self, language: Language) -> &'static str {
        if language == Language::ZhTw {
            return super::copy::zh_tw_ref(self.word(Language::ZhCn));
        }
        match (language, self) {
            (Language::En, Self::Usable) => "Available",
            (Language::En, Self::NeedsSetup) => "Needs setup",
            (Language::En, Self::NotRunning) => "Not running",
            (Language::En, Self::NotInstalled) => "Not installed",
            (Language::En, Self::NeedsUpdate) => "Needs update",
            (Language::En, Self::Disabled) => "Disabled",
            (Language::En, Self::Checking) => "Checking…",
            (Language::ZhCn, Self::Usable) => "可用",
            (Language::ZhCn, Self::NeedsSetup) => "需要设置",
            (Language::ZhCn, Self::NotRunning) => "未运行",
            (Language::ZhCn, Self::NotInstalled) => "未安装",
            (Language::ZhCn, Self::NeedsUpdate) => "需要更新",
            (Language::ZhCn, Self::Disabled) => "已关闭",
            (Language::ZhCn, Self::Checking) => "检查中…",
            (Language::ZhTw, _) => unreachable!("derived above"),
        }
    }
}

/// The agent's display name: discovery's, else a built-in one.
pub fn name(id: &str, agents: &[AgentDiscoveryInfo]) -> String {
    agents
        .iter()
        .find(|a| a.id == id)
        .map(|a| a.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback_name(id).to_string())
}

pub fn fallback_name(id: &str) -> &str {
    match id {
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "deepseek" => "DeepSeek",
        "t3" => "T3 Code",
        "fx" => "FX",
        "qoder" | "qodercli" | "qoder-cli" => "Qoder",
        "pi" => "Pi",
        "claude" | "claude-code" => "Claude",
        "antigravity" | "agy" => "Antigravity",
        "cursor" => "Cursor",
        other => other,
    }
}

/// Canonical brand id used for colour + mark lookup.
pub fn brand_id(id: &str) -> &str {
    match id {
        "qodercli" | "qoder-cli" => "qoder",
        "claude-code" => "claude",
        "agy" => "antigravity",
        other => other,
    }
}

/// The brand mark Icon for an agent kind.
pub fn mark_icon(kind: &str) -> &'static Icon {
    match brand_id(kind) {
        "opencode" => &agent_marks::OPENCODE,
        "deepseek" => &agent_marks::DEEPSEEK,
        "t3" => &agent_marks::T3,
        "codex" => &agent_marks::CODEX,
        "pi" => &agent_marks::PI,
        "fx" => &agent_marks::FX,
        "qoder" => &agent_marks::QODER,
        "cursor" => &agent_marks::CURSOR,
        "claude" => &agent_marks::CLAUDE,
        "antigravity" => &agent_marks::ANTIGRAVITY,
        _ => &agent_marks::AGENT,
    }
}

/// Brand colour for `id` in the current scheme.
pub fn color(id: &str, dark: bool) -> Color {
    brand::brand(brand_id(id), dark)
}

/// The agent's brand mark at `size` pt, tinted with its brand colour.
pub fn mark(id: &str, size: f32, dark: bool) -> El {
    icon(mark_icon(id), size).color(color(id, dark))
}

/// Same mark, muted (for disabled / idle agents).
pub fn mark_idle(id: &str, size: f32, muted: Color) -> El {
    icon(mark_icon(id), size).color(muted)
}

/// A small filled disc in the agent's brand colour.
#[allow(dead_code)]
pub fn dot(id: &str, dark: bool) -> El {
    rune::el().size(8.0, 8.0).radius(4.0).fill(color(id, dark))
}

/// Agents in the gateway's order, usable first.
pub fn by_standing(agents: &[AgentDiscoveryInfo]) -> Vec<(AgentDiscoveryInfo, Standing)> {
    let mut list: Vec<_> = agents
        .iter()
        .map(|a| (a.clone(), Standing::of(a)))
        .collect();
    list.sort_by_key(|(a, s)| order_key(&a.id, *s));
    list
}

fn order_key(id: &str, standing: Standing) -> (bool, usize) {
    let rank = AGENT_PREFERENCE
        .iter()
        .position(|p| *p == id)
        .unwrap_or(AGENT_PREFERENCE.len());
    (standing != Standing::Usable, rank)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_maps_to_the_six_words() {
        use AgentAvailability::*;
        for (status, want) in [
            (Connected, Standing::Usable),
            (Reachable, Standing::Usable),
            (Unconfigured, Standing::NeedsSetup),
            (Offline, Standing::NotRunning),
            (NotInstalled, Standing::NotInstalled),
            (Unsupported, Standing::NeedsUpdate),
            (Disabled, Standing::Disabled),
            (Unknown, Standing::Checking),
        ] {
            assert_eq!(Standing::from_status(true, &status), want, "{status:?}");
        }
        assert_eq!(Standing::from_status(false, &Connected), Standing::Disabled);
    }

    #[test]
    fn usable_agents_come_first_in_gateway_order() {
        let mut ids = vec![
            ("claude", Standing::Usable),
            ("t3", Standing::NeedsSetup),
            ("codex", Standing::Usable),
            ("opencode", Standing::NotRunning),
        ];
        ids.sort_by_key(|(id, s)| order_key(id, *s));
        let ids: Vec<_> = ids.into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["codex", "claude", "opencode", "t3"]);
        assert_eq!(name("claude", &[]), "Claude");
        assert_eq!(brand_id("agy"), "antigravity");
        assert_eq!(mark_icon("who").name, agent_marks::AGENT.name);
        assert_eq!(mark_icon("claude-code").name, agent_marks::CLAUDE.name);
    }
}
