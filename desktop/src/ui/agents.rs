//! How an agent is named and shown: its status word (the gateway's six
//! words, from `AgentAvailability`), its short badge, and its brand class.
//! The colours themselves live in `styles.css` as `--agent-<id>`.

use blinc_layout::prelude::*;
use muqun_gateway::platform::discovery::{AgentAvailability, AgentDiscoveryInfo};

use super::strings::Strings;
use super::text::ui_text;
use crate::config::Language;
use crate::hub::AGENT_PREFERENCE;

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

    pub fn word(self, strings: &Strings) -> &'static str {
        match (strings.language, self) {
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
        }
    }

    pub fn class(self) -> &'static str {
        match self {
            Self::Usable => "mq-standing--usable",
            Self::Checking => "mq-standing--checking",
            _ => "mq-standing--blocked",
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

fn fallback_name(id: &str) -> &str {
    match id {
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "deepseek" => "DeepSeek",
        "t3" => "T3 Code",
        "fx" => "FX",
        "qoder" => "Qoder",
        "pi" => "Pi",
        "claude" => "Claude",
        "antigravity" => "Antigravity",
        "cursor" => "Cursor",
        other => other,
    }
}

/// Two letters for the badge.
pub fn short(id: &str) -> &'static str {
    match id {
        "codex" => "Cx",
        "opencode" => "OC",
        "deepseek" => "DS",
        "t3" => "T3",
        "fx" => "FX",
        "qoder" => "Qd",
        "pi" => "Pi",
        "claude" => "Cl",
        "antigravity" => "AG",
        "cursor" => "Cu",
        _ => "··",
    }
}

/// The CSS class carrying an agent's brand colour; unknown ids get a
/// neutral one.
pub fn brand_class(id: &str) -> String {
    if AGENT_PREFERENCE.contains(&id) {
        format!("mq-agent--{id}")
    } else {
        "mq-agent--other".to_string()
    }
}

/// The agent's two-letter badge in its brand colour.
pub fn badge(id: &str) -> Div {
    div()
        .class("mq-agent-badge")
        .class(brand_class(id))
        .flex_shrink_0()
        .child(
            ui_text(short(id))
                .size(10.0)
                .semibold()
                .no_wrap()
                .no_cursor(),
        )
}

/// A small dot in the agent's brand colour.
pub fn dot(id: &str) -> Div {
    div()
        .class("mq-agent-dot")
        .class(brand_class(id))
        .w(8.0)
        .h(8.0)
        .rounded(4.0)
        .flex_shrink_0()
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

/// Usable first, then the gateway's preference order.
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
            let label = format!("{status:?}");
            assert_eq!(Standing::from_status(true, &status), want, "{label}");
        }
        assert_eq!(
            Standing::from_status(false, &Connected),
            Standing::Disabled,
            "a disabled agent is disabled whatever its probe says"
        );
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
        assert_eq!(
            name("claude", &[]),
            "Claude",
            "falls back to the built-in name"
        );
        assert_eq!(brand_class("nope"), "mq-agent--other");
    }
}
