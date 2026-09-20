use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelVariantInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variants: Option<Vec<ModelVariantInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<serde_json::Value>,
    /// `Model.Info.enabled`: OpenCode knows the model but it is switched off.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `alpha`, `beta`, `deprecated` or `active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// `Agent.Info.hidden`: built-ins like `title` and `compaction` that a
    /// picker must not offer.
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `Skill.Info.slash`: the skill is offered as a slash entry. The app's
    /// slash menu lists only these; the rest are for the agent to find.
    /// Omitted in the payload means `false`.
    #[serde(default)]
    pub slash: bool,
    /// `Skill.Info.autoinvoke`: the agent may activate this one by itself.
    #[serde(default)]
    pub autoinvoke: bool,
}

/// One model as a provider lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelInfo {
    pub id: String,
    pub name: String,
    /// `false` when OpenCode has the model but it is switched off; the app
    /// greys it out rather than hiding it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<ModelVariantInfo>,
    /// `Model.Info.limit`: `{context, input?, output}` verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<serde_json::Value>,
    /// `alpha`, `beta`, `deprecated` or `active`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A provider and the models it offers. Auth is not the app's business -- the
/// user configures OpenCode on the host -- so only the activation state is
/// carried, for a "configure OpenCode on the host" hint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    /// `auto`, `enabled` or `disabled`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ProviderModelInfo>,
}

/// A slash command OpenCode will run for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// What OpenCode would choose for a session the app does not pick for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CatalogDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<super::session::ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentCatalog {
    pub models: Vec<ModelInfo>,
    pub agents: Vec<AgentInfo>,
    pub mcp: Vec<McpServerInfo>,
    #[serde(default)]
    pub skills: Vec<SkillInfo>,
    #[serde(default)]
    pub providers: Vec<ProviderInfo>,
    #[serde(default)]
    pub commands: Vec<CommandInfo>,
    #[serde(default)]
    pub defaults: CatalogDefaults,
}
