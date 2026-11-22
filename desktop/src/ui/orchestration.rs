//! MAO (Multi-Agent Orchestration · 智能编排) UI, pipeline state machine,
//! and automated Coder ⇄ Reviewer execution loop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use muqun_gateway::agents::{
    order_efforts, AgentPart, AgentSessionId, AgentSessionInfo, AgentSessionStatus, ModelRef,
    TimelineRole, ToolCallStatus,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use rune::{col, el, icon, row, text, Color, El, TextEdit, TextStyle, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, Menu, MenuEntry, MenuItem,
    Placement, ScrollBar, SegmentedControl, TextInput,
};

use crate::config::{ConfigEdit, Language, MaoPrefs, MaoRoleConfig, MaoSessionOverride};
use crate::hub::CatalogView;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, Deliver, Target};

use super::agents::{self, Standing};
use super::copy::{tr, tr_ref};
use super::labels::{bare_catalog_id, catalog_model_info, format_model_name, readable_catalog_id};
use super::parts::glyphs;
use super::{ago, composer, now_ms, Mode, OpenMenu, Slot, ToastLevel, UiState};

const MAX_W: f32 = 1760.0;
const STACK_AT: f32 = 900.0;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MaoRole {
    pub agent: String,
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MaoPhase {
    Coding { round: u8 },
    Reviewing { round: u8 },
    Approved { round: u8, summary: String },
    ChangesRequested { round: u8, feedback: String },
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MaoStepKind {
    Coding,
    Review,
    Rework,
    Verdict,
    Failover,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MaoStep {
    pub kind: MaoStepKind,
    pub round: u8,
    pub title: String,
    pub detail: String,
    pub status: &'static str,
    pub asid: Option<String>,
    pub time_ms: u64,
}

impl<'de> serde::Deserialize<'de> for MaoStep {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Wire {
            kind: MaoStepKind,
            round: u8,
            title: String,
            detail: String,
            status: String,
            asid: Option<String>,
            time_ms: u64,
        }
        let wire = Wire::deserialize(deserializer)?;
        let status = match wire.status.as_str() {
            "completed" | "done" => "completed",
            "warning" => "warning",
            "failed" => "failed",
            "cancelled" => "cancelled",
            _ => "running",
        };
        Ok(Self {
            kind: wire.kind,
            round: wire.round,
            title: wire.title,
            detail: wire.detail,
            status,
            asid: wire.asid,
            time_ms: wire.time_ms,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MaoDeliverableFile {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MaoFinding {
    pub severity: String,
    pub location: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MaoRun {
    pub id: String,
    pub computer: String,
    pub goal: String,
    pub cwd: String,
    pub coder: MaoRole,
    pub reviewer: MaoRole,
    pub coder_asid: String,
    pub reviewer_asid: Option<String>,
    pub pending_reviewer_token: Option<u64>,
    pub pending_review_diff: bool,
    pub pre_review_msg_id: Option<String>,
    pub base_commit: Option<String>,
    pub base_commits: std::collections::BTreeMap<String, String>,
    pub last_diff_fingerprint: Option<String>,
    pub snapshot_retries: u8,
    pub round: u8,
    pub max_rounds: u8,
    pub phase: MaoPhase,
    pub files: Vec<MaoDeliverableFile>,
    pub coder_summary: String,
    pub review_summary: String,
    pub findings: Vec<MaoFinding>,
    pub steps: Vec<MaoStep>,
    pub expanded: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
}

impl Default for MaoRun {
    fn default() -> Self {
        Self {
            id: String::new(),
            computer: "local".into(),
            goal: String::new(),
            cwd: String::new(),
            coder: MaoRole {
                agent: String::new(),
                model: None,
                machine: None,
            },
            reviewer: MaoRole {
                agent: String::new(),
                model: None,
                machine: None,
            },
            coder_asid: String::new(),
            reviewer_asid: None,
            pending_reviewer_token: None,
            pending_review_diff: false,
            pre_review_msg_id: None,
            base_commit: None,
            base_commits: std::collections::BTreeMap::new(),
            last_diff_fingerprint: None,
            snapshot_retries: 0,
            round: 1,
            max_rounds: 2,
            phase: MaoPhase::Coding { round: 1 },
            files: Vec::new(),
            coder_summary: String::new(),
            review_summary: String::new(),
            findings: Vec::new(),
            steps: Vec::new(),
            expanded: false,
            created_ms: 0,
            updated_ms: 0,
        }
    }
}

impl MaoRun {
    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            MaoPhase::Coding { .. } | MaoPhase::Reviewing { .. }
        )
    }

    pub fn needs_attention(&self) -> bool {
        matches!(
            self.phase,
            MaoPhase::ChangesRequested { .. } | MaoPhase::Failed { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Active,
    Review,
    Done,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::Active, Filter::Review, Filter::Done];

    fn word(self, language: Language) -> &'static str {
        match self {
            Filter::All => tr_ref(language, "All"),
            Filter::Active => tr_ref(language, "Running"),
            Filter::Review => tr_ref(language, "Needs decision"),
            Filter::Done => tr_ref(language, "Completed"),
        }
    }

    fn keeps(self, run: &MaoRun) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => run.is_active(),
            Filter::Review => run.needs_attention(),
            Filter::Done => matches!(self_phase(&run.phase), PhaseBucket::Done),
        }
    }
}

enum PhaseBucket {
    Done,
    Other,
}

fn self_phase(phase: &MaoPhase) -> PhaseBucket {
    match phase {
        MaoPhase::Approved { .. } | MaoPhase::Cancelled => PhaseBucket::Done,
        _ => PhaseBucket::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuSection {
    Agent,
    Model,
    Effort,
    Machine,
    Rounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaoMenuAnchor {
    StartComposer,
    SessionComposer,
    Page,
    Settings,
    Dock(Slot),
}

impl MaoMenuAnchor {
    pub fn slot(self) -> Slot {
        match self {
            Self::StartComposer | Self::Page | Self::Settings => Slot::Start,
            Self::SessionComposer => Slot::Session,
            Self::Dock(slot) => slot,
        }
    }

    fn pill_key(self) -> &'static str {
        match self {
            Self::StartComposer => "mao-reviewer-pill-start",
            Self::SessionComposer => "mao-reviewer-pill-session",
            Self::Page => "mao-reviewer-pill-page",
            Self::Settings => "mao-reviewer-pill-settings",
            Self::Dock(Slot::Start) => "mao-reviewer-pill-dock-start",
            Self::Dock(Slot::Session) => "mao-reviewer-pill-dock-session",
        }
    }

    fn menu_key(self) -> &'static str {
        match self {
            Self::StartComposer => "mao-reviewer-start",
            Self::SessionComposer => "mao-reviewer-session",
            Self::Page => "mao-reviewer-page",
            Self::Settings => "mao-reviewer-settings",
            Self::Dock(Slot::Start) => "mao-reviewer-dock-start",
            Self::Dock(Slot::Session) => "mao-reviewer-dock-session",
        }
    }
}

#[derive(Debug)]
pub struct View {
    pub runs: Vec<MaoRun>,
    pub selected_run: Option<String>,
    pub filter: Filter,
    pub menu_section: Option<MenuSection>,
    pub pool_add_agent: Option<String>,
    pub reviewer_catalogs: HashMap<String, Arc<CatalogView>>,
    pub catalog_requests: HashMap<u64, String>,
    pub synced_computers: HashSet<String>,
    pub enabled_sessions: HashSet<String>,
    pub gateway_start_tokens: HashMap<u64, (String, u64)>,
    pub pending_start_runs: HashMap<String, (String, u64)>,
    pub preserved_titles: HashMap<String, String>,
    pub last_gateway_poll_ms: u64,
    pub rework_note: TextEdit,
    pub rework_note_run: Option<String>,
    pub next_seq: u64,
}

fn runs_state_path() -> Option<PathBuf> {
    let (_, data_dir) = crate::platform::directories().ok()?;
    Some(data_dir.join("orchestration-runs.json"))
}

fn load_persisted_runs() -> Vec<MaoRun> {
    if cfg!(test) {
        return Vec::new();
    }
    let Some(path) = runs_state_path() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn persist_runs(s: &UiState) {
    if cfg!(test) || s.demo {
        return;
    }
    let Some(path) = runs_state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let len = s.orchestration.runs.len();
    let keep = &s.orchestration.runs[len.saturating_sub(40)..];
    if let Ok(json) = serde_json::to_string_pretty(keep) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

impl Default for View {
    fn default() -> Self {
        let runs = load_persisted_runs();
        let next_seq = runs
            .iter()
            .filter_map(|r| r.id.strip_prefix("mao-")?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        let selected_run = runs.last().map(|r| r.id.clone());
        let enabled_sessions = runs
            .iter()
            .filter(|r| {
                r.is_active() && !r.coder_asid.is_empty() && !r.coder_asid.starts_with("new-")
            })
            .map(|r| r.coder_asid.clone())
            .collect();
        Self {
            runs,
            selected_run,
            filter: Filter::default(),
            menu_section: None,
            pool_add_agent: None,
            reviewer_catalogs: HashMap::new(),
            catalog_requests: HashMap::new(),
            synced_computers: HashSet::new(),
            enabled_sessions,
            gateway_start_tokens: HashMap::new(),
            pending_start_runs: HashMap::new(),
            preserved_titles: HashMap::new(),
            last_gateway_poll_ms: 0,
            rework_note: TextEdit::new(""),
            rework_note_run: None,
            next_seq,
        }
    }
}

impl View {
    pub fn is_session_enabled(&self, asid: &str) -> bool {
        self.enabled_sessions.contains(asid)
    }

    pub fn tracked_asids(&self) -> Vec<AgentSessionId> {
        let mut out = Vec::new();
        for run in &self.runs {
            if !run.coder_asid.is_empty() && !run.coder_asid.starts_with("new-") {
                let id = AgentSessionId(run.coder_asid.clone());
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            if let Some(ref rev) = run.reviewer_asid {
                if !rev.is_empty() && !rev.starts_with("new-") {
                    let id = AgentSessionId(rev.clone());
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
            }
        }
        out
    }

    pub fn is_tracked(&self, asid: &str) -> bool {
        self.runs
            .iter()
            .any(|run| run.coder_asid == asid || run.reviewer_asid.as_deref() == Some(asid))
    }

    pub fn run_for_session(&self, asid: &str) -> Option<&MaoRun> {
        self.runs
            .iter()
            .rfind(|run| run.coder_asid == asid || run.reviewer_asid.as_deref() == Some(asid))
    }

    pub fn active_count(&self) -> usize {
        self.runs.iter().filter(|r| r.is_active()).count()
    }

    pub fn row_working(&self, asid: &str) -> bool {
        self.run_for_session(asid).is_some_and(|r| r.is_active())
    }

    pub fn row_status(&self, asid: &str, language: Language) -> Option<String> {
        let run = self.run_for_session(asid)?;
        let label = match &run.phase {
            MaoPhase::Coding { round } => {
                format!("{} #{}", tr_ref(language, "MAO · Coding"), round)
            }
            MaoPhase::Reviewing { round } => {
                format!("{} #{}", tr_ref(language, "MAO · Reviewing"), round)
            }
            MaoPhase::Approved { .. } => tr(language, "MAO · Approved"),
            MaoPhase::ChangesRequested { .. } => tr(language, "MAO · Changes requested"),
            MaoPhase::Failed { .. } => tr(language, "MAO · Failed"),
            MaoPhase::Cancelled => tr(language, "MAO · Stopped"),
        };
        Some(label)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approved { summary: String },
    ChangesRequested { feedback: String },
}

pub fn parse_review_findings(reply: &str) -> Vec<MaoFinding> {
    let mut findings = Vec::new();
    for raw in reply.lines() {
        let trimmed = raw
            .trim()
            .trim_start_matches('-')
            .trim_start_matches('*')
            .trim();
        let upper = trimmed.to_ascii_uppercase();
        let Some(pos) = upper.find("MAO_ISSUE:") else {
            continue;
        };
        let mut rest = trimmed[pos + "MAO_ISSUE:".len()..].trim();
        let mut severity = "medium".to_string();
        if let Some(after_open) = rest.strip_prefix('[') {
            if let Some(close) = after_open.find(']') {
                let tag = after_open[..close].trim().to_ascii_uppercase();
                severity = match tag.as_str() {
                    "HIGH" | "CRITICAL" | "BLOCKER" => "high",
                    "LOW" | "NIT" | "INFO" => "low",
                    _ => "medium",
                }
                .to_string();
                rest = after_open[close + 1..].trim();
            }
        }
        let (location, message) = if let Some((loc, msg)) = rest.split_once('|') {
            (loc.trim(), msg.trim())
        } else if let Some((loc, msg)) = rest.split_once('—') {
            (loc.trim(), msg.trim())
        } else if let Some((loc, msg)) = rest.split_once(" - ") {
            (loc.trim(), msg.trim())
        } else {
            ("workspace", rest)
        };
        if !message.is_empty() {
            findings.push(MaoFinding {
                severity,
                location: if location.is_empty() {
                    "workspace".into()
                } else {
                    location.to_string()
                },
                message: concise_excerpt(message, 220),
            });
        }
        if findings.len() >= 16 {
            break;
        }
    }
    findings
}

pub fn clean_review_summary(reply: &str, max_chars: usize) -> String {
    let lines: Vec<&str> = reply
        .lines()
        .filter(|line| {
            let upper = line.trim().to_ascii_uppercase();
            !upper.contains("MAO_VERDICT:") && !upper.contains("MAO_ISSUE:")
        })
        .collect();
    let cleaned = lines.join("\n");
    if !cleaned.trim().is_empty() {
        return concise_excerpt(&cleaned, max_chars);
    }
    let findings = parse_review_findings(reply);
    if !findings.is_empty() {
        let joined = findings
            .into_iter()
            .map(|f| {
                format!(
                    "[{}] {}: {}",
                    f.severity.to_uppercase(),
                    f.location,
                    f.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return concise_excerpt(&joined, max_chars);
    }
    String::new()
}

pub fn parse_review_verdict(reply: &str) -> ReviewVerdict {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return ReviewVerdict::Approved {
            summary: String::new(),
        };
    }

    // Prefer an explicit last `MAO_VERDICT:` marker so quoted instructions
    // earlier in a transcript do not override the actual final decision.
    let mut explicit: Option<bool> = None;
    for raw in trimmed.lines() {
        let upper = raw.trim().to_ascii_uppercase();
        if let Some(pos) = upper.find("MAO_VERDICT:") {
            let tail = upper[pos + "MAO_VERDICT:".len()..].trim();
            if tail.starts_with("CHANGES_REQUESTED")
                || tail.starts_with("REQUEST_CHANGES")
                || tail.starts_with("REJECTED")
            {
                explicit = Some(false);
            } else if tail.starts_with("APPROVED") || tail.starts_with("PASS") {
                explicit = Some(true);
            }
        }
    }

    let summary = clean_review_summary(trimmed, 420);
    let feedback = clean_review_summary(trimmed, 1600);
    let findings = parse_review_findings(trimmed);
    let low_only = !findings.is_empty() && findings.iter().all(|f| f.severity == "low");
    match explicit {
        Some(true) => ReviewVerdict::Approved { summary },
        Some(false) if low_only => ReviewVerdict::Approved { summary },
        Some(false) => ReviewVerdict::ChangesRequested { feedback },
        None => {
            let lower = trimmed.to_ascii_lowercase();
            let negative = [
                "changes requested",
                "changes_requested",
                "must fix",
                "blocking issue",
                "blocker:",
                "need to fix",
                "issues found",
                "需要修改",
                "存在问题",
                "不通过",
                "打回修改",
            ];
            if !low_only
                && negative
                    .iter()
                    .any(|needle| lower.contains(needle) || trimmed.contains(needle))
            {
                ReviewVerdict::ChangesRequested { feedback }
            } else {
                ReviewVerdict::Approved { summary }
            }
        }
    }
}

fn diff_fingerprint(files: &[(String, usize, usize)], diff_summary: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut sorted: Vec<_> = files.to_vec();
    sorted.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sorted.hash(&mut hasher);
    diff_summary.trim().hash(&mut hasher);
    format!("{:016x}:{}", hasher.finish(), sorted.len())
}

fn concise_excerpt(text: &str, max_chars: usize) -> String {
    let clean = text.trim();
    match clean.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", clean[..idx].trim_end()),
        None => clean.to_string(),
    }
}

fn usable_agents(s: &UiState) -> Vec<AgentDiscoveryInfo> {
    let list: Vec<AgentDiscoveryInfo> = agents::by_standing(&s.agents)
        .into_iter()
        .filter_map(|(a, standing)| (standing == Standing::Usable).then_some(a))
        .collect();
    if list.is_empty() {
        s.agents.clone()
    } else {
        list
    }
}

pub fn default_reviewer_agent(s: &UiState, coder_agent: &str) -> String {
    let usable = usable_agents(s);
    if usable.is_empty() {
        return coder_agent.to_string();
    }
    for preferred in ["codex", "claude", "opencode"] {
        if preferred != coder_agent && usable.iter().any(|a| a.id == preferred) {
            return preferred.to_string();
        }
    }
    usable
        .iter()
        .find(|a| a.id != coder_agent)
        .or_else(|| usable.first())
        .map(|a| a.id.clone())
        .unwrap_or_else(|| coder_agent.to_string())
}

fn active_coder_agent(s: &UiState, slot: Slot) -> String {
    match slot {
        Slot::Session => s
            .current()
            .map(|info| info.agent_id.clone())
            .unwrap_or_else(|| s.start_agent.clone()),
        Slot::Start => s.start_agent.clone(),
    }
}

fn slot_asid(s: &UiState, slot: Slot) -> Option<&str> {
    match slot {
        Slot::Start => None,
        Slot::Session => s
            .selected
            .as_deref()
            .filter(|id| !id.is_empty() && !id.starts_with("new-")),
    }
}

fn has_session_config_override(s: &UiState, asid: Option<&str>) -> bool {
    asid.and_then(|id| s.cfg.ui.mao.session_configs.get(id))
        .is_some_and(|ov| !ov.is_empty())
}

pub fn effective_max_rounds(s: &UiState, asid: Option<&str>) -> u8 {
    asid.and_then(|id| s.cfg.ui.mao.session_configs.get(id))
        .and_then(|ov| ov.max_rounds)
        .unwrap_or(s.cfg.ui.mao.max_rounds)
        .clamp(1, 5)
}

pub fn effective_auto_failover(s: &UiState, asid: Option<&str>) -> bool {
    asid.and_then(|id| s.cfg.ui.mao.session_configs.get(id))
        .and_then(|ov| ov.auto_failover)
        .unwrap_or(s.cfg.ui.mao.auto_failover)
}

pub fn effective_reviewer_for(s: &UiState, coder_agent: &str, asid: Option<&str>) -> MaoRole {
    let usable = usable_agents(s);
    let session_cfg = asid
        .and_then(|id| s.cfg.ui.mao.session_configs.get(id))
        .and_then(|ov| ov.reviewer.as_ref());
    if let Some(cfg) = session_cfg.or(s.cfg.ui.mao.reviewer.as_ref()) {
        let valid = usable.is_empty() || usable.iter().any(|a| a.id == cfg.agent);
        if valid {
            let model = match cfg.model.as_ref() {
                Some(m) if !model_in_options(s, &cfg.agent, &m.model_id) => {
                    preferred_cheap_model(s, &cfg.agent)
                }
                other => other.cloned(),
            };
            return MaoRole {
                agent: cfg.agent.clone(),
                model,
                machine: cfg.machine.clone(),
            };
        }
    }
    MaoRole {
        agent: default_reviewer_agent(s, coder_agent),
        model: None,
        machine: None,
    }
}

pub fn effective_reviewer(s: &UiState, coder_agent: &str) -> MaoRole {
    effective_reviewer_for(s, coder_agent, None)
}

fn catalog_for_agent<'a>(s: &'a UiState, agent_id: &str) -> Option<&'a Arc<CatalogView>> {
    if let Some(view) = s.orchestration.reviewer_catalogs.get(agent_id) {
        return Some(view);
    }
    if s.start_agent == agent_id {
        if let Some(view) = s.start_catalog.as_ref() {
            return Some(view);
        }
    }
    if let Some(info) = s.current() {
        if info.agent_id == agent_id {
            if let Some(view) = s.catalog.as_ref() {
                return Some(view);
            }
        }
    }
    None
}

pub fn ensure_reviewer_catalog(s: &mut UiState, agent_id: &str) {
    if agent_id.is_empty() || catalog_for_agent(s, agent_id).is_some() {
        return;
    }
    if s.orchestration
        .catalog_requests
        .values()
        .any(|existing| existing == agent_id)
    {
        return;
    }
    let request = s.next_token;
    s.next_token += 1;
    s.orchestration
        .catalog_requests
        .insert(request, agent_id.to_string());
    s.send(Command::Compose(ComposeCommand::Catalog {
        request,
        agent: agent_id.to_string(),
        directory: None,
        asid: None,
    }));
}

pub fn on_catalog(s: &mut UiState, request: u64, agent: &str, view: Arc<CatalogView>) {
    let requested = s.orchestration.catalog_requests.remove(&request);
    let key = requested.as_deref().unwrap_or(agent).to_string();
    if !key.is_empty() {
        s.orchestration.reviewer_catalogs.insert(key.clone(), view);
        if s.cfg.ui.mao.reviewer.as_ref().is_some_and(|r| {
            r.agent == key
                && (r.model.is_none()
                    || r.model
                        .as_ref()
                        .is_some_and(|m| !model_in_options(s, &key, &m.model_id)))
        }) {
            if let Some(cheap) = preferred_cheap_model(s, &key) {
                let mut next = s.cfg.ui.mao.clone();
                if let Some(rev) = next.reviewer.as_mut() {
                    rev.model = Some(cheap);
                }
                save_mao_prefs(s, next);
            }
        }
    }
}

fn model_in_options(s: &UiState, agent_id: &str, model_id: &str) -> bool {
    let opts = model_options(s, agent_id);
    opts.is_empty() || opts.iter().any(|(_, id, _, _)| id == model_id)
}

fn cheap_model_score(id: &str, name: &str) -> u8 {
    let hay = format!("{} {}", id.to_ascii_lowercase(), name.to_ascii_lowercase());
    if hay.contains("nano") {
        0
    } else if hay.contains("mini")
        || hay.contains("haiku")
        || hay.contains("flash")
        || hay.contains("5.6-luna")
    {
        1
    } else if hay.contains("lite")
        || hay.contains("small")
        || hay.contains("deepseek")
        || hay.contains("luna")
    {
        2
    } else if hay.contains("terra") || hay.contains("sonnet") || hay.contains("codex") {
        3
    } else if hay.contains("sol") {
        4
    } else if hay.contains("astra")
        || hay.contains("opus")
        || hay.contains("pro")
        || hay.contains("max")
    {
        5
    } else {
        4
    }
}

pub fn preferred_cheap_model(s: &UiState, agent_id: &str) -> Option<ModelRef> {
    let mut options = model_options(s, agent_id);
    if options.is_empty() {
        return None;
    }
    options.sort_by_key(|(_, id, name, _)| cheap_model_score(id, name));
    let (provider_id, model_id, _, variants) = options.into_iter().next()?;
    let variant = ["low", "minimal", "medium"]
        .into_iter()
        .find(|tier| variants.iter().any(|v| v.eq_ignore_ascii_case(tier)))
        .map(|s| s.to_string())
        .or_else(|| variants.first().cloned());
    Some(ModelRef {
        provider_id,
        model_id,
        variant,
    })
}

fn model_options(s: &UiState, agent_id: &str) -> Vec<(String, String, String, Vec<String>)> {
    if let Some(view) = catalog_for_agent(s, agent_id) {
        let mut out = Vec::new();
        for (provider, model_id, display) in composer::catalog_models(view) {
            let model_ref = ModelRef {
                provider_id: provider.clone(),
                model_id: model_id.clone(),
                variant: None,
            };
            let mut variants: Vec<String> = catalog_model_info(view, &model_ref)
                .and_then(|info| info.variants.as_ref())
                .map(|list| list.iter().map(|v| v.id.clone()).collect())
                .unwrap_or_default();
            order_efforts(&mut variants, |v| Some(v.as_str()));
            out.push((provider, model_id, display, variants));
        }
        if !out.is_empty() {
            return out;
        }
    }

    s.agents
        .iter()
        .find(|a| a.id == agent_id)
        .map(|a| {
            a.models
                .iter()
                .map(|m| {
                    let mut tiers = m.reasoning_effort_tiers.clone();
                    order_efforts(&mut tiers, |v| Some(v.as_str()));
                    let name = if m.name.trim().is_empty() {
                        readable_catalog_id(&m.id)
                    } else {
                        m.name.clone()
                    };
                    (m.provider_id.clone(), m.id.clone(), name, tiers)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn resolved_reviewer_model<'a>(
    s: &'a UiState,
    role: &'a MaoRole,
) -> (Option<ModelRef>, Option<&'a str>) {
    if let Some(m) = &role.model {
        if model_in_options(s, &role.agent, &m.model_id) {
            let display = catalog_for_agent(s, &role.agent)
                .and_then(|v| catalog_model_info(v, m))
                .map(|i| i.name.as_str());
            return (Some(m.clone()), display);
        }
    }
    if let Some(cheap) = preferred_cheap_model(s, &role.agent) {
        let display = catalog_for_agent(s, &role.agent)
            .and_then(|v| catalog_model_info(v, &cheap))
            .map(|i| i.name.as_str());
        return (Some(cheap), display);
    }
    if let Some(view) = catalog_for_agent(s, &role.agent) {
        if let Some(def) = view.catalog.defaults.model.as_ref() {
            let display = catalog_model_info(view, def).map(|i| i.name.as_str());
            return (Some(def.clone()), display);
        }
    }
    (None, None)
}

fn variants_for_model(s: &UiState, agent_id: &str, model: Option<&ModelRef>) -> Vec<String> {
    let Some(m) = model else {
        return Vec::new();
    };
    if let Some(view) = catalog_for_agent(s, agent_id) {
        if let Some(info) = catalog_model_info(view, m) {
            let mut variants: Vec<String> = info
                .variants
                .as_ref()
                .map(|list| list.iter().map(|v| v.id.clone()).collect())
                .unwrap_or_default();
            order_efforts(&mut variants, |v| Some(v.as_str()));
            return variants;
        }
    }
    let mut tiers = s
        .agents
        .iter()
        .find(|a| a.id == agent_id)
        .and_then(|a| {
            a.models.iter().find(|entry| {
                entry.id == m.model_id || bare_catalog_id(&entry.id) == bare_catalog_id(&m.model_id)
            })
        })
        .map(|entry| entry.reasoning_effort_tiers.clone())
        .unwrap_or_default();
    order_efforts(&mut tiers, |v| Some(v.as_str()));
    tiers
}

fn save_mao_prefs(s: &mut UiState, prefs: MaoPrefs) {
    s.edit(ConfigEdit::Mao(prefs));
}

pub fn is_session_mao_enabled(s: &UiState, asid: &str) -> bool {
    match s.cfg.ui.mao.session_overrides.get(asid) {
        Some(&on) => on,
        None => s.orchestration.is_session_enabled(asid),
    }
}

fn set_session_mao_override(s: &mut UiState, asid: &str, enabled: bool) {
    if asid.is_empty() || asid.starts_with("new-") {
        return;
    }
    let mut next = s.cfg.ui.mao.clone();
    next.session_overrides.insert(asid.to_string(), enabled);
    while next.session_overrides.len() > 256 {
        let Some(oldest) = next
            .session_overrides
            .keys()
            .find(|k| k.as_str() != asid)
            .cloned()
        else {
            break;
        };
        next.session_overrides.remove(&oldest);
    }
    save_mao_prefs(s, next);
}

pub fn is_slot_enabled(s: &UiState, slot: Slot) -> bool {
    match slot {
        Slot::Start => s.cfg.ui.mao.enabled,
        Slot::Session => s
            .selected
            .as_deref()
            .is_some_and(|asid| is_session_mao_enabled(s, asid)),
    }
}

fn ensure_default_reviewer_prefs(s: &mut UiState, slot: Slot) {
    let coder = active_coder_agent(s, slot);
    let role = effective_reviewer(s, &coder);
    ensure_reviewer_catalog(s, &role.agent);
    if s.cfg.ui.mao.reviewer.is_none() {
        let mut next = s.cfg.ui.mao.clone();
        next.reviewer = Some(MaoRoleConfig {
            agent: role.agent,
            model: role.model,
            machine: role.machine,
        });
        save_mao_prefs(s, next);
    }
}

fn toggle_enabled(s: &mut UiState, slot: Slot) {
    match slot {
        Slot::Start => {
            let mut next = s.cfg.ui.mao.clone();
            next.enabled = !next.enabled;
            let enabling = next.enabled;
            save_mao_prefs(s, next);
            if enabling {
                ensure_default_reviewer_prefs(s, Slot::Start);
            }
        }
        Slot::Session => {
            let Some(asid) = s.selected.clone() else {
                return;
            };
            let next_on = !is_session_mao_enabled(s, &asid);
            if next_on {
                s.orchestration.enabled_sessions.insert(asid.clone());
                set_session_mao_override(s, &asid, true);
                ensure_default_reviewer_prefs(s, Slot::Session);
                if !asid.starts_with("new-") {
                    s.auto_approve.insert(asid.clone(), true);
                    s.send(Command::Compose(
                        crate::model::compose::ComposeCommand::SetAutoApprove {
                            asid: AgentSessionId(asid),
                            enabled: true,
                        },
                    ));
                }
            } else {
                s.orchestration.enabled_sessions.remove(&asid);
                set_session_mao_override(s, &asid, false);
            }
        }
    }
}

enum SlotConfigTarget<'a> {
    Global(&'a mut MaoPrefs),
    Session(&'a mut MaoSessionOverride),
}

fn mutate_slot_config(s: &mut UiState, slot: Slot, f: impl FnOnce(SlotConfigTarget<'_>)) {
    let asid = slot_asid(s, slot).map(str::to_string);
    let mut next = s.cfg.ui.mao.clone();
    if let Some(id) = asid {
        let entry = next.session_configs.entry(id.clone()).or_default();
        f(SlotConfigTarget::Session(entry));
        if next
            .session_configs
            .get(&id)
            .is_some_and(MaoSessionOverride::is_empty)
        {
            next.session_configs.remove(&id);
        } else {
            while next.session_configs.len() > 256 {
                let Some(oldest) = next
                    .session_configs
                    .keys()
                    .find(|k| k.as_str() != id)
                    .cloned()
                else {
                    break;
                };
                next.session_configs.remove(&oldest);
            }
        }
    } else {
        f(SlotConfigTarget::Global(&mut next));
    }
    save_mao_prefs(s, next);
}

fn reset_session_mao_config(s: &mut UiState, anchor: MaoMenuAnchor) {
    let slot = anchor.slot();
    if let Some(asid) = slot_asid(s, slot).map(str::to_string) {
        let mut next = s.cfg.ui.mao.clone();
        if next.session_configs.remove(&asid).is_some() {
            save_mao_prefs(s, next);
        }
    }
    s.menu = Some(OpenMenu::MaoReviewer(anchor));
}

fn set_reviewer_agent(s: &mut UiState, anchor: MaoMenuAnchor, agent_id: String) {
    let slot = anchor.slot();
    ensure_reviewer_catalog(s, &agent_id);
    let coder = active_coder_agent(s, slot);
    let asid = slot_asid(s, slot).map(str::to_string);
    let machine = effective_reviewer_for(s, &coder, asid.as_deref()).machine;
    let role = MaoRoleConfig {
        agent: agent_id,
        model: None,
        machine,
    };
    mutate_slot_config(s, slot, |target| match target {
        SlotConfigTarget::Session(ov) => ov.reviewer = Some(role),
        SlotConfigTarget::Global(global) => global.reviewer = Some(role),
    });
    s.orchestration.menu_section = Some(MenuSection::Model);
    s.menu = Some(OpenMenu::MaoReviewer(anchor));
}

fn set_reviewer_model(
    s: &mut UiState,
    anchor: MaoMenuAnchor,
    provider_id: String,
    model_id: String,
) {
    let slot = anchor.slot();
    let coder = active_coder_agent(s, slot);
    let asid = slot_asid(s, slot).map(str::to_string);
    let current = effective_reviewer_for(s, &coder, asid.as_deref());
    let catalog = catalog_for_agent(s, &current.agent).map(|arc| arc.as_ref());
    let picked = composer::choose_model(catalog, current.model.as_ref(), provider_id, model_id);
    let role = MaoRoleConfig {
        agent: current.agent,
        model: Some(picked),
        machine: current.machine,
    };
    mutate_slot_config(s, slot, |target| match target {
        SlotConfigTarget::Session(ov) => ov.reviewer = Some(role),
        SlotConfigTarget::Global(global) => global.reviewer = Some(role),
    });
    s.orchestration.menu_section = Some(MenuSection::Effort);
    s.menu = Some(OpenMenu::MaoReviewer(anchor));
}

fn set_reviewer_effort(s: &mut UiState, anchor: MaoMenuAnchor, variant: Option<String>) {
    let slot = anchor.slot();
    let coder = active_coder_agent(s, slot);
    let asid = slot_asid(s, slot).map(str::to_string);
    let current = effective_reviewer_for(s, &coder, asid.as_deref());
    let (resolved, _) = resolved_reviewer_model(s, &current);
    let Some(mut model) = resolved else {
        return;
    };
    model.variant = variant;
    let role = MaoRoleConfig {
        agent: current.agent,
        model: Some(model),
        machine: current.machine,
    };
    mutate_slot_config(s, slot, |target| match target {
        SlotConfigTarget::Session(ov) => ov.reviewer = Some(role),
        SlotConfigTarget::Global(global) => global.reviewer = Some(role),
    });
    s.menu = Some(OpenMenu::MaoReviewer(anchor));
}

fn set_reviewer_machine(s: &mut UiState, anchor: MaoMenuAnchor, machine: Option<String>) {
    let slot = anchor.slot();
    let coder = active_coder_agent(s, slot);
    let asid = slot_asid(s, slot).map(str::to_string);
    let current = effective_reviewer_for(s, &coder, asid.as_deref());
    let role = MaoRoleConfig {
        agent: current.agent,
        model: current.model,
        machine,
    };
    mutate_slot_config(s, slot, |target| match target {
        SlotConfigTarget::Session(ov) => ov.reviewer = Some(role),
        SlotConfigTarget::Global(global) => global.reviewer = Some(role),
    });
    s.menu = Some(OpenMenu::MaoReviewer(anchor));
}

fn set_max_rounds(s: &mut UiState, anchor: MaoMenuAnchor, rounds: u8) {
    let slot = anchor.slot();
    let clamped = rounds.clamp(1, 5);
    mutate_slot_config(s, slot, |target| match target {
        SlotConfigTarget::Session(ov) => ov.max_rounds = Some(clamped),
        SlotConfigTarget::Global(global) => global.max_rounds = clamped,
    });
    s.menu = Some(OpenMenu::MaoReviewer(anchor));
}

fn toggle_auto_failover(s: &mut UiState, anchor: MaoMenuAnchor) {
    let slot = anchor.slot();
    let asid = slot_asid(s, slot).map(str::to_string);
    let next_val = !effective_auto_failover(s, asid.as_deref());
    mutate_slot_config(s, slot, |target| match target {
        SlotConfigTarget::Session(ov) => ov.auto_failover = Some(next_val),
        SlotConfigTarget::Global(global) => global.auto_failover = next_val,
    });
    s.menu = Some(OpenMenu::MaoReviewer(anchor));
}

pub fn on_baseline_captured(
    s: &mut UiState,
    run_id: &str,
    base_commit: Option<String>,
    base_commits: std::collections::BTreeMap<String, String>,
) {
    if base_commits.is_empty() && base_commit.is_none() {
        return;
    }
    if let Some(run) = s.orchestration.runs.iter_mut().find(|r| r.id == run_id) {
        if run.base_commits.is_empty() && !base_commits.is_empty() {
            run.base_commits = base_commits;
        }
        if run.base_commit.is_none() {
            run.base_commit = base_commit.or_else(|| run.base_commits.values().next().cloned());
        }
        persist_runs(s);
    }
}

pub fn on_start_submitted(
    s: &mut UiState,
    pending_asid: &str,
    prompt: &str,
    cwd: &str,
    coder_agent: &str,
    coder_model: Option<ModelRef>,
) {
    if !s.cfg.ui.mao.enabled {
        return;
    }
    s.orchestration
        .enabled_sessions
        .insert(pending_asid.to_string());
    let reviewer = effective_reviewer(s, coder_agent);
    ensure_reviewer_catalog(s, &reviewer.agent);
    let seq = {
        s.orchestration.next_seq += 1;
        s.orchestration.next_seq
    };
    let now = now_ms();
    let max_rounds = s.cfg.ui.mao.max_rounds.clamp(1, 5);
    let lang = s.cfg.general.language;
    let coder_name = s.agent_name(coder_agent);
    let id = format!("mao-{seq}");
    let goal_str = prompt.trim().to_string();
    let cwd_str = cwd.trim().to_string();
    let run = MaoRun {
        id: id.clone(),
        computer: s.current_computer().to_string(),
        goal: goal_str.clone(),
        cwd: cwd_str.clone(),
        coder: MaoRole {
            agent: coder_agent.to_string(),
            model: coder_model,
            machine: None,
        },
        reviewer,
        coder_asid: pending_asid.to_string(),
        reviewer_asid: None,
        pending_reviewer_token: None,
        pending_review_diff: false,
        pre_review_msg_id: None,
        base_commit: None,
        base_commits: std::collections::BTreeMap::new(),
        last_diff_fingerprint: None,
        snapshot_retries: 0,
        round: 1,
        max_rounds,
        phase: MaoPhase::Coding { round: 1 },
        files: Vec::new(),
        coder_summary: String::new(),
        review_summary: String::new(),
        findings: Vec::new(),
        steps: vec![MaoStep {
            kind: MaoStepKind::Coding,
            round: 1,
            title: format!("{} #1 · {coder_name}", tr_ref(lang, "Coding")),
            detail: concise_excerpt(prompt, 180),
            status: "running",
            asid: Some(pending_asid.to_string()),
            time_ms: now,
        }],
        expanded: false,
        created_ms: now,
        updated_ms: now,
    };
    s.orchestration.runs.push(run);
    s.orchestration.selected_run = Some(id.clone());
    if !cwd_str.is_empty() {
        s.send(Command::MaoCaptureBaseline {
            run_id: id,
            cwd: cwd_str,
            goal: goal_str,
        });
    }
    persist_runs(s);
}

pub fn preserve_session_title(s: &mut UiState, asid: &str) {
    if asid.is_empty()
        || asid.starts_with("new-")
        || s.orchestration.preserved_titles.contains_key(asid)
    {
        return;
    }
    if let Some(title) = s
        .session_by_id(asid)
        .map(|info| info.title.trim().to_string())
        .filter(|t| !t.is_empty() && t != asid && !t.starts_with("[MAO "))
    {
        s.orchestration
            .preserved_titles
            .insert(asid.to_string(), title);
    }
}

pub fn on_user_renamed_session(s: &mut UiState, asid: &str, title: &str) {
    let trimmed = title.trim();
    if !trimmed.is_empty() && s.orchestration.preserved_titles.contains_key(asid) {
        s.orchestration
            .preserved_titles
            .insert(asid.to_string(), trimmed.to_string());
    }
}

pub fn on_session_submitted(s: &mut UiState, asid: &str, prompt: &str) {
    if !is_session_mao_enabled(s, asid) {
        return;
    }
    preserve_session_title(s, asid);
    // Manual prompts inside a dedicated Reviewer session should not fork nested MAO runs.
    if s.orchestration
        .runs
        .iter()
        .any(|r| r.reviewer_asid.as_deref() == Some(asid))
    {
        return;
    }

    let now = now_ms();
    let lang = s.cfg.general.language;
    if let Some(existing_idx) = s
        .orchestration
        .runs
        .iter()
        .rposition(|r| r.coder_asid == asid)
    {
        if !s.orchestration.runs[existing_idx].is_active() {
            let existing_coder = s.orchestration.runs[existing_idx].coder.agent.clone();
            let coder_name = agents::name(&existing_coder, &s.agents);
            let session_reviewer = effective_reviewer_for(s, &existing_coder, Some(asid));
            let session_rounds = effective_max_rounds(s, Some(asid));
            let existing = &mut s.orchestration.runs[existing_idx];
            existing.reviewer = session_reviewer;
            existing.round = 1;
            existing.max_rounds = session_rounds;
            existing.goal = prompt.trim().to_string();
            existing.phase = MaoPhase::Coding { round: 1 };
            existing.pending_review_diff = false;
            existing.base_commits.clear();
            existing.base_commit = None;
            existing.last_diff_fingerprint = None;
            existing.snapshot_retries = 0;
            existing.files.clear();
            existing.coder_summary.clear();
            existing.review_summary.clear();
            existing.findings.clear();
            existing.updated_ms = now;
            existing.steps.push(MaoStep {
                kind: MaoStepKind::Coding,
                round: 1,
                title: format!("{} #1 · {coder_name}", tr_ref(lang, "Follow-up coding")),
                detail: concise_excerpt(prompt, 180),
                status: "running",
                asid: Some(asid.to_string()),
                time_ms: now,
            });
            let run_id = existing.id.clone();
            let run_cwd = existing.cwd.clone();
            let run_goal = existing.goal.clone();
            s.orchestration.selected_run = Some(run_id.clone());
            if !run_cwd.trim().is_empty() {
                s.send(Command::MaoCaptureBaseline {
                    run_id,
                    cwd: run_cwd,
                    goal: run_goal,
                });
            }
            persist_runs(s);
        } else {
            s.orchestration.runs[existing_idx].updated_ms = now;
        }
        return;
    }

    let info = s.session_by_id(asid).cloned();
    let coder_agent = info
        .as_ref()
        .map(|i| i.agent_id.clone())
        .unwrap_or_else(|| s.start_agent.clone());
    let coder_model = info.as_ref().and_then(|i| i.model.clone());
    let cwd = info
        .as_ref()
        .and_then(|i| i.directory.clone())
        .unwrap_or_default();
    let reviewer = effective_reviewer_for(s, &coder_agent, Some(asid));
    ensure_reviewer_catalog(s, &reviewer.agent);
    let max_rounds = effective_max_rounds(s, Some(asid));
    let seq = {
        s.orchestration.next_seq += 1;
        s.orchestration.next_seq
    };
    let coder_name = s.agent_name(&coder_agent);
    let id = format!("mao-{seq}");
    let goal_str = prompt.trim().to_string();
    s.orchestration.runs.push(MaoRun {
        id: id.clone(),
        computer: s.current_computer().to_string(),
        goal: goal_str.clone(),
        cwd: cwd.clone(),
        coder: MaoRole {
            agent: coder_agent,
            model: coder_model,
            machine: None,
        },
        reviewer,
        coder_asid: asid.to_string(),
        reviewer_asid: None,
        pending_reviewer_token: None,
        pending_review_diff: false,
        pre_review_msg_id: None,
        base_commit: None,
        base_commits: std::collections::BTreeMap::new(),
        last_diff_fingerprint: None,
        snapshot_retries: 0,
        round: 1,
        max_rounds,
        phase: MaoPhase::Coding { round: 1 },
        files: Vec::new(),
        coder_summary: String::new(),
        review_summary: String::new(),
        findings: Vec::new(),
        steps: vec![MaoStep {
            kind: MaoStepKind::Coding,
            round: 1,
            title: format!("{} #1 · {coder_name}", tr_ref(lang, "Coding")),
            detail: concise_excerpt(prompt, 180),
            status: "running",
            asid: Some(asid.to_string()),
            time_ms: now,
        }],
        expanded: false,
        created_ms: now,
        updated_ms: now,
    });
    if !cwd.trim().is_empty() {
        s.send(Command::MaoCaptureBaseline {
            run_id: id.clone(),
            cwd,
            goal: goal_str,
        });
    }
    s.orchestration.selected_run = Some(id);
    s.sync_watch();
    persist_runs(s);
}

pub fn on_session_created(s: &mut UiState, token: u64, info: &AgentSessionInfo) {
    let pending_id = format!("new-{token}");
    let real_id = info.asid.0.clone();
    let mut promoted_mao = false;
    if s.orchestration.enabled_sessions.remove(&pending_id)
        && s.cfg.ui.mao.session_overrides.get(&real_id) != Some(&false)
    {
        s.orchestration.enabled_sessions.insert(real_id.clone());
        promoted_mao = true;
    }
    let mut matched = false;
    let mut lineage_pairs: Vec<(String, String)> = Vec::new();
    let mut created_reviewer: Option<String> = None;
    let mut baseline_captures: Vec<(String, String, String)> = Vec::new();

    for run in &mut s.orchestration.runs {
        if run.coder_asid == pending_id {
            run.coder_asid = real_id.clone();
            if s.cfg.ui.mao.session_overrides.get(&real_id) != Some(&false) {
                s.orchestration.enabled_sessions.insert(real_id.clone());
                promoted_mao = true;
            }
            if run.cwd.is_empty() {
                if let Some(dir) = &info.directory {
                    run.cwd = dir.clone();
                }
            }
            if run.base_commits.is_empty() && !run.cwd.trim().is_empty() {
                baseline_captures.push((run.id.clone(), run.cwd.clone(), run.goal.clone()));
            }
            for step in &mut run.steps {
                if step.asid.as_deref() == Some(pending_id.as_str()) {
                    step.asid = Some(real_id.clone());
                }
            }
            run.updated_ms = now_ms();
            matched = true;
        }
        if run.pending_reviewer_token == Some(token) {
            run.pending_reviewer_token = None;
            run.reviewer_asid = Some(real_id.clone());
            if let Some(step) = run
                .steps
                .iter_mut()
                .rev()
                .find(|st| st.kind == MaoStepKind::Review && st.asid.is_none())
            {
                step.asid = Some(real_id.clone());
            }
            run.updated_ms = now_ms();
            if !run.coder_asid.starts_with("new-") {
                lineage_pairs.push((real_id.clone(), run.coder_asid.clone()));
            }
            created_reviewer = Some(real_id.clone());
            matched = true;
        }
    }

    for (run_id, cwd, goal) in baseline_captures {
        s.send(Command::MaoCaptureBaseline { run_id, cwd, goal });
    }
    for (child, parent) in &lineage_pairs {
        s.remember_subagent_context(child, Some(parent));
    }
    if let Some(rev_id) = created_reviewer {
        s.timelines.entry(rev_id.clone()).or_default();
        if s.mode == Mode::Session {
            if let Some((_, coder_parent)) = lineage_pairs.first() {
                if !coder_parent.starts_with("new-") && coder_parent != &rev_id {
                    s.open_detail(coder_parent, &rev_id);
                }
            }
        }
    }
    if promoted_mao {
        set_session_mao_override(s, &real_id, true);
    }
    if matched {
        s.auto_approve.insert(real_id, true);
        s.sync_watch();
        persist_runs(s);
    }
}

fn latest_assistant_entry(
    timelines: &HashMap<String, super::timeline::SessionTimeline>,
    asid: &str,
) -> Option<(String, String)> {
    let tl = timelines.get(asid)?;
    let last_item = tl.items.iter().rev().find(|item| {
        item.role == TimelineRole::Assistant && matches!(item.part, AgentPart::Text { .. })
    })?;
    let target_msg = last_item.message_id.clone();
    let mut text = String::new();
    for item in &tl.items {
        if item.role == TimelineRole::Assistant && item.message_id == target_msg {
            if let AgentPart::Text { text: chunk } = &item.part {
                if !text.is_empty() && !chunk.is_empty() {
                    text.push('\n');
                }
                text.push_str(chunk);
            }
        }
    }
    if text.trim().is_empty() {
        return None;
    }
    Some((target_msg, text))
}

pub fn on_session_status(s: &mut UiState, asid: &str, status: AgentSessionStatus) {
    let lang = s.cfg.general.language;
    let now = now_ms();

    let mut gateway_reads: Vec<String> = Vec::new();
    for run in &s.orchestration.runs {
        if !run.id.starts_with("o_") || !run.is_active() {
            continue;
        }
        let matches_role = run.coder_asid == asid || run.reviewer_asid.as_deref() == Some(asid);
        if matches_role
            && matches!(
                status,
                AgentSessionStatus::Idle
                    | AgentSessionStatus::Failed
                    | AgentSessionStatus::Interrupted
            )
            && !gateway_reads.contains(&run.id)
        {
            gateway_reads.push(run.id.clone());
        }
    }
    for run_id in &gateway_reads {
        request_gateway_read(s, run_id);
    }

    // Check Coder state transitions first (local standalone runs only).
    let mut prepare_cmd: Option<Command> = None;
    for run in &mut s.orchestration.runs {
        if run.id.starts_with("o_") || run.coder_asid != asid {
            continue;
        }
        if let MaoPhase::Coding { .. } = run.phase {
            match status {
                AgentSessionStatus::Idle if !run.pending_review_diff => {
                    run.pending_review_diff = true;
                    run.updated_ms = now;
                    if let Some(step) = run
                        .steps
                        .iter_mut()
                        .rev()
                        .find(|st| matches!(st.kind, MaoStepKind::Coding | MaoStepKind::Rework))
                    {
                        step.status = "completed";
                    }
                    prepare_cmd = Some(Command::MaoPrepareReview {
                        run_id: run.id.clone(),
                        coder_asid: run.coder_asid.clone(),
                        cwd: run.cwd.clone(),
                        goal: run.goal.clone(),
                        base_commit: run.base_commit.clone(),
                        base_commits: run.base_commits.clone(),
                    });
                }
                AgentSessionStatus::Failed => {
                    run.pending_review_diff = false;
                    run.phase = MaoPhase::Failed {
                        error: tr(lang, "Coder session failed."),
                    };
                    run.updated_ms = now;
                    if let Some(step) = run.steps.last_mut() {
                        if step.status == "running" {
                            step.status = "failed";
                        }
                    }
                }
                AgentSessionStatus::Interrupted => {
                    run.pending_review_diff = false;
                    run.phase = MaoPhase::Cancelled;
                    run.updated_ms = now;
                    if let Some(step) = run.steps.last_mut() {
                        if step.status == "running" {
                            step.status = "cancelled";
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(cmd) = prepare_cmd {
        s.send(cmd);
    }

    // Check Reviewer state transitions.
    #[derive(Clone)]
    enum ReviewerOutcome {
        NeedSnapshot(String),
        Approved {
            reviewer_name: String,
            round: u8,
        },
        Rework {
            coder_asid: String,
            prompt: String,
            echo: String,
            round: u8,
            reviewer_name: String,
        },
        MaxRoundsReached {
            reviewer_name: String,
            round: u8,
        },
    }

    let mut outcomes: Vec<ReviewerOutcome> = Vec::new();
    for run in &mut s.orchestration.runs {
        if run.reviewer_asid.as_deref() != Some(asid) {
            continue;
        }
        let MaoPhase::Reviewing { round } = run.phase else {
            continue;
        };
        let reviewer_name = agents::name(&run.reviewer.agent, &s.agents);
        match status {
            AgentSessionStatus::Idle => {
                let latest = latest_assistant_entry(&s.timelines, asid);
                let fresh = latest
                    .as_ref()
                    .is_some_and(|(msg_id, _)| run.pre_review_msg_id.as_ref() != Some(msg_id));
                if !fresh {
                    run.snapshot_retries = run.snapshot_retries.saturating_add(1);
                    if run.snapshot_retries == 1 {
                        outcomes.push(ReviewerOutcome::NeedSnapshot(asid.to_string()));
                    } else {
                        let fallback = if run.review_summary.is_empty() {
                            tr(lang, "Waiting for approval")
                        } else {
                            run.review_summary.clone()
                        };
                        run.phase = MaoPhase::ChangesRequested {
                            round,
                            feedback: fallback.clone(),
                        };
                        run.updated_ms = now;
                        if let Some(step) = run
                            .steps
                            .iter_mut()
                            .rev()
                            .find(|st| st.kind == MaoStepKind::Review && st.round == round)
                        {
                            step.status = "warning";
                            step.detail = concise_excerpt(&fallback, 220);
                        }
                        outcomes.push(ReviewerOutcome::MaxRoundsReached {
                            reviewer_name,
                            round,
                        });
                    }
                    continue;
                }
                run.snapshot_retries = 0;
                let (_, reply) = latest.unwrap();
                run.findings = parse_review_findings(&reply);
                run.review_summary = clean_review_summary(&reply, 600);
                match parse_review_verdict(&reply) {
                    ReviewVerdict::Approved { summary } => {
                        run.phase = MaoPhase::Approved {
                            round,
                            summary: summary.clone(),
                        };
                        run.updated_ms = now;
                        if let Some(step) = run
                            .steps
                            .iter_mut()
                            .rev()
                            .find(|st| st.kind == MaoStepKind::Review && st.round == round)
                        {
                            step.status = "completed";
                            step.detail = clean_review_summary(&reply, 240);
                        }
                        run.steps.push(MaoStep {
                            kind: MaoStepKind::Verdict,
                            round,
                            title: format!("{} #{round}", tr_ref(lang, "Review approved")),
                            detail: if summary.is_empty() {
                                tr(lang, "Reviewer approved the changes.")
                            } else {
                                summary
                            },
                            status: "completed",
                            asid: Some(asid.to_string()),
                            time_ms: now,
                        });
                        outcomes.push(ReviewerOutcome::Approved {
                            reviewer_name,
                            round,
                        });
                    }
                    ReviewVerdict::ChangesRequested { feedback } => {
                        if let Some(step) = run
                            .steps
                            .iter_mut()
                            .rev()
                            .find(|st| st.kind == MaoStepKind::Review && st.round == round)
                        {
                            step.status = "warning";
                            step.detail = concise_excerpt(&feedback, 240);
                        }
                        if round < run.max_rounds {
                            let next_round = round + 1;
                            run.round = next_round;
                            run.phase = MaoPhase::Coding { round: next_round };
                            run.updated_ms = now;
                            run.steps.push(MaoStep {
                                kind: MaoStepKind::Rework,
                                round: next_round,
                                title: format!(
                                    "{} #{next_round} · {}",
                                    tr_ref(lang, "Rework"),
                                    agents::name(&run.coder.agent, &s.agents)
                                ),
                                detail: concise_excerpt(&feedback, 220),
                                status: "running",
                                asid: Some(run.coder_asid.clone()),
                                time_ms: now,
                            });
                            let prompt = build_rework_prompt(
                                &run.goal,
                                next_round,
                                run.max_rounds,
                                &reviewer_name,
                                &feedback,
                                &run.findings,
                            );
                            let echo = format!(
                                "[MAO · Review Round {round} Feedback from {reviewer_name}]\n\n{}{}",
                                concise_excerpt(&feedback, 600),
                                format_findings_block(&run.findings)
                            );
                            outcomes.push(ReviewerOutcome::Rework {
                                coder_asid: run.coder_asid.clone(),
                                prompt,
                                echo,
                                round: next_round,
                                reviewer_name,
                            });
                        } else {
                            run.phase = MaoPhase::ChangesRequested {
                                round,
                                feedback: feedback.clone(),
                            };
                            run.updated_ms = now;
                            run.steps.push(MaoStep {
                                kind: MaoStepKind::Verdict,
                                round,
                                title: format!(
                                    "{} ({}/{})",
                                    tr_ref(lang, "Changes requested"),
                                    round,
                                    run.max_rounds
                                ),
                                detail: concise_excerpt(&feedback, 240),
                                status: "warning",
                                asid: Some(asid.to_string()),
                                time_ms: now,
                            });
                            outcomes.push(ReviewerOutcome::MaxRoundsReached {
                                reviewer_name,
                                round,
                            });
                        }
                    }
                }
            }
            AgentSessionStatus::Failed => {
                if let Some(step) = run.steps.last_mut() {
                    if step.status == "running" {
                        step.status = if run.files.is_empty() {
                            "failed"
                        } else {
                            "warning"
                        };
                    }
                }
                if !run.files.is_empty() {
                    run.phase = MaoPhase::ChangesRequested {
                        round,
                        feedback: tr(lang, "Reviewer session failed."),
                    };
                    outcomes.push(ReviewerOutcome::MaxRoundsReached {
                        reviewer_name,
                        round,
                    });
                } else {
                    run.phase = MaoPhase::Failed {
                        error: tr(lang, "Reviewer session failed."),
                    };
                }
                run.updated_ms = now;
            }
            AgentSessionStatus::Interrupted => {
                run.phase = MaoPhase::Cancelled;
                run.updated_ms = now;
                if let Some(step) = run.steps.last_mut() {
                    if step.status == "running" {
                        step.status = "cancelled";
                    }
                }
            }
            _ => {}
        }
    }

    for outcome in outcomes {
        match outcome {
            ReviewerOutcome::NeedSnapshot(rev_asid) => {
                s.send(Command::OpenTile(AgentSessionId(rev_asid)));
            }
            ReviewerOutcome::Approved {
                reviewer_name,
                round,
            } => {
                s.sync_watch();
                persist_runs(s);
                s.notify(
                    ToastLevel::Success,
                    tr(lang, "MAO review approved"),
                    Some(format!("{reviewer_name} · #{round}")),
                );
            }
            ReviewerOutcome::Rework {
                coder_asid,
                prompt,
                echo,
                round,
                reviewer_name,
            } => {
                let token = s.next_token;
                s.next_token += 1;
                s.echo_sent(&coder_asid, token, echo, &[], None);
                let switching_from_reviewer = s
                    .orchestration
                    .runs
                    .iter()
                    .find(|r| r.coder_asid == coder_asid)
                    .and_then(|r| r.reviewer_asid.clone())
                    .filter(|rev| s.selected.as_deref() == Some(rev.as_str()));
                s.send(Command::Compose(ComposeCommand::Submit {
                    token,
                    target: Target::Session(AgentSessionId(coder_asid.clone())),
                    deliver: Deliver::Prompt(prompt),
                    attachments: Vec::new(),
                    delivery: None,
                    auto_approve: true,
                }));
                if let Some(rev_id) = switching_from_reviewer {
                    s.open_session(&coder_asid);
                    if rev_id != coder_asid {
                        s.open_detail(&coder_asid, &rev_id);
                    }
                }
                s.notify(
                    ToastLevel::Info,
                    tr(lang, "MAO rework dispatched"),
                    Some(format!("{reviewer_name} → #{round}")),
                );
            }
            ReviewerOutcome::MaxRoundsReached {
                reviewer_name,
                round,
            } => {
                s.sync_watch();
                persist_runs(s);
                s.notify(
                    ToastLevel::Warning,
                    tr(lang, "MAO review requested changes"),
                    Some(format!("{reviewer_name} · #{round}")),
                );
            }
        }
    }
    if s.orchestration.is_tracked(asid) {
        persist_runs(s);
    }
}

pub fn on_review_ready(
    s: &mut UiState,
    run_id: &str,
    mut diff_summary: String,
    files: Vec<(String, usize, usize)>,
) {
    let lang = s.cfg.general.language;
    let now = now_ms();
    let Some(idx) = s.orchestration.runs.iter().position(|r| r.id == run_id) else {
        return;
    };
    let (coder_id_lookup, run_cwd_lookup) = {
        let run = &s.orchestration.runs[idx];
        (run.coder_asid.clone(), run.cwd.clone())
    };
    let mut merged: Vec<muqun_gateway::orchestration::ChangedFile> = Vec::new();
    let incoming_files = files
        .into_iter()
        .map(
            |(path, additions, deletions)| muqun_gateway::orchestration::ChangedFile {
                path,
                additions,
                deletions,
            },
        )
        .collect();
    muqun_gateway::orchestration::merge_changed_files(&run_cwd_lookup, &mut merged, incoming_files);
    if let Some(tl) = s.timelines.get(&coder_id_lookup) {
        let turn_start = tl
            .items
            .iter()
            .rposition(|item| item.role == TimelineRole::User)
            .map_or(0, |idx| idx + 1);
        let (tl_files, tl_evidence) = muqun_gateway::orchestration::extract_timeline_evidence(
            tl.items[turn_start..].iter().map(|i| &i.part),
        );
        muqun_gateway::orchestration::merge_changed_files(&run_cwd_lookup, &mut merged, tl_files);
        if !tl_evidence.trim().is_empty()
            && (diff_summary.trim().is_empty()
                || tl_evidence.contains("tasks.")
                || tl_evidence.contains("exec.")
                || tl_evidence.contains("muqun"))
        {
            if !diff_summary.trim().is_empty() {
                diff_summary.push_str("\n\n");
            }
            diff_summary.push_str(tl_evidence.trim());
        }
    }
    let files: Vec<(String, usize, usize)> = merged
        .into_iter()
        .map(|f| (f.path, f.additions, f.deletions))
        .collect();
    let (
        coder_asid,
        existing_rev,
        goal,
        round,
        max_rounds,
        reviewer,
        cwd,
        skip_review,
        stagnant_rework,
        prev_review_summary,
        prev_findings,
    ) = {
        let run = &mut s.orchestration.runs[idx];
        run.pending_review_diff = false;
        let round = match run.phase {
            MaoPhase::Coding { round } => round,
            _ => return,
        };
        let empty_diff = files.is_empty() && diff_summary.trim().is_empty() && run.files.is_empty();
        let fp = (!empty_diff).then(|| diff_fingerprint(&files, &diff_summary));
        let stagnant_rework = round > 1
            && fp
                .as_ref()
                .is_some_and(|cur| run.last_diff_fingerprint.as_deref() == Some(cur.as_str()));
        if !stagnant_rework {
            if let Some(f) = fp {
                run.last_diff_fingerprint = Some(f);
            }
        }
        if !files.is_empty() {
            run.files = files
                .into_iter()
                .map(|(path, additions, deletions)| MaoDeliverableFile {
                    path,
                    additions,
                    deletions,
                })
                .collect();
        }
        run.updated_ms = now;
        (
            run.coder_asid.clone(),
            run.reviewer_asid.clone(),
            run.goal.clone(),
            round,
            run.max_rounds,
            run.reviewer.clone(),
            run.cwd.clone(),
            empty_diff,
            stagnant_rework,
            run.review_summary.clone(),
            run.findings.clone(),
        )
    };

    let recorded_coder_reply = latest_assistant_entry(&s.timelines, &coder_asid)
        .map(|(_, text)| concise_excerpt(&text, 2200));

    if skip_review {
        let msg = tr(lang, "No code changes detected; skipped code review.");
        let run = &mut s.orchestration.runs[idx];
        if let Some(reply) = recorded_coder_reply {
            run.coder_summary = concise_excerpt(&reply, 600);
        }
        run.phase = MaoPhase::Approved {
            round,
            summary: msg.clone(),
        };
        run.review_summary = msg.clone();
        run.findings.clear();
        run.snapshot_retries = 0;
        run.steps.push(MaoStep {
            kind: MaoStepKind::Verdict,
            round,
            title: tr(lang, "Skipped review (no code changes)"),
            detail: msg,
            status: "completed",
            asid: Some(coder_asid),
            time_ms: now,
        });
        s.sync_watch();
        persist_runs(s);
        s.notify(
            ToastLevel::Success,
            tr(lang, "MAO review approved"),
            Some(tr(lang, "Skipped review (no code changes)")),
        );
        return;
    }

    if stagnant_rework {
        let reviewer_name = agents::name(&reviewer.agent, &s.agents);
        let feedback = if prev_review_summary.is_empty() {
            "Rework produced identical workspace diff; awaiting user decision.".into()
        } else {
            prev_review_summary
        };
        let run = &mut s.orchestration.runs[idx];
        if let Some(reply) = recorded_coder_reply {
            run.coder_summary = concise_excerpt(&reply, 600);
        }
        run.phase = MaoPhase::ChangesRequested {
            round,
            feedback: feedback.clone(),
        };
        run.snapshot_retries = 0;
        run.steps.push(MaoStep {
            kind: MaoStepKind::Verdict,
            round,
            title: format!(
                "{} ({round}/{max_rounds})",
                tr_ref(lang, "Changes requested")
            ),
            detail: concise_excerpt(&feedback, 220),
            status: "warning",
            asid: Some(coder_asid),
            time_ms: now,
        });
        s.sync_watch();
        persist_runs(s);
        s.notify(
            ToastLevel::Warning,
            tr(lang, "MAO review requested changes"),
            Some(format!("{reviewer_name} · #{round}")),
        );
        return;
    }

    let coder_summary = recorded_coder_reply
        .as_deref()
        .unwrap_or("(No text response recorded yet.)")
        .to_string();
    let prompt = build_review_prompt(
        &goal,
        round,
        max_rounds,
        &coder_summary,
        &diff_summary,
        &prev_findings,
    );
    let pre_msg_id = existing_rev
        .as_deref()
        .and_then(|asid| latest_assistant_entry(&s.timelines, asid).map(|(id, _)| id));
    let reviewer_name = agents::name(&reviewer.agent, &s.agents);
    let token = s.next_token;
    s.next_token += 1;

    {
        let run = &mut s.orchestration.runs[idx];
        run.phase = MaoPhase::Reviewing { round };
        run.snapshot_retries = 0;
        if let Some(reply) = recorded_coder_reply {
            run.coder_summary = concise_excerpt(&reply, 600);
        }
        run.pre_review_msg_id = pre_msg_id;
        run.steps.push(MaoStep {
            kind: MaoStepKind::Review,
            round,
            title: format!("{} #{round} · {reviewer_name}", tr_ref(lang, "Code review")),
            detail: if diff_summary.trim().is_empty() {
                tr(lang, "Reviewing implementation and changes…")
            } else {
                concise_excerpt(&diff_summary, 220)
            },
            status: "running",
            asid: existing_rev.clone(),
            time_ms: now,
        });
        if existing_rev.is_none() {
            run.pending_reviewer_token = Some(token);
        }
    }

    if let Some(rev_asid) = existing_rev {
        s.echo_sent(
            &rev_asid,
            token,
            format!("[MAO · Code Review Round {round}/{max_rounds}]\n\n{diff_summary}"),
            &[],
            None,
        );
        s.send(Command::Compose(ComposeCommand::Submit {
            token,
            target: Target::Session(AgentSessionId(rev_asid.clone())),
            deliver: Deliver::Prompt(prompt),
            attachments: Vec::new(),
            delivery: None,
            auto_approve: true,
        }));
        if s.mode == Mode::Session && !coder_asid.starts_with("new-") && coder_asid != rev_asid {
            s.open_detail(&coder_asid, &rev_asid);
        }
    } else {
        s.send(Command::Compose(ComposeCommand::Submit {
            token,
            target: Target::New {
                agent: reviewer.agent,
                cwd: PathBuf::from(cwd),
                model: reviewer.model,
                mode: None,
                worktree: false,
            },
            deliver: Deliver::Prompt(prompt),
            attachments: Vec::new(),
            delivery: None,
            auto_approve: true,
        }));
    }
    s.sync_watch();
    persist_runs(s);
}

fn build_review_prompt(
    goal: &str,
    round: u8,
    max_rounds: u8,
    coder_summary: &str,
    diff_summary: &str,
    prev_findings: &[MaoFinding],
) -> String {
    let diff_block = if diff_summary.trim().is_empty() {
        "(No local git diff returned cleanly; check remote/MCP task evidence, session tool logs, or workspace files before concluding.)"
    } else {
        diff_summary.trim()
    };
    let delta_section = if round > 1 && !prev_findings.is_empty() {
        let items = prev_findings
            .iter()
            .take(8)
            .map(|f| {
                format!(
                    "- [{}] {}: {}",
                    f.severity.to_ascii_uppercase(),
                    f.location,
                    f.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("Previous round issues to verify first (delta focus):\n{items}\n\n")
    } else {
        String::new()
    };
    format!(
        "You are the designated **Read-Only Code Reviewer** in an automated Coder ⇄ Reviewer pipeline (Review Round {round}/{max_rounds}).\n\n\
         STRICT RULES:\n\
         - Read-only verifier: NEVER edit, create, delete, or commit source files in the workspace. Only read files/diffs/task outputs and run read-only verification checks or tests.\n\
         - Tasks in Muqun may modify single/multiple local git repos, external filesystem paths, OR remote linked computers via Muqun MCP (`tasks.delegate`, `exec.run`). NEVER assume no work happened just because local `git diff` is empty when MCP/remote execution evidence or tool logs show real changes or test results.\n\
         - NEVER ask interactive questions or wait for user confirmation; make a decisive verdict from the evidence and read-only checks.\n\
         - Check for correctness bugs, edge cases, regressions, security issues, missing tests, and violations of project rules.\n\n\
         User task goal:\n{goal}\n\n\
         {delta_section}\
         Coder summary:\n{coder_summary}\n\n\
         Changed files / Execution evidence:\n{diff_block}\n\n\
         Output format:\n\
         - Provide a concise review summary first.\n\
         - For each concrete issue found, include one line formatted exactly as:\n\
           `MAO_ISSUE: [HIGH|MEDIUM|LOW] path:line | clear issue description and expected fix`\n\
         - Your final line MUST start with exactly one of:\n\
           - `MAO_VERDICT: APPROVED` — if the implementation is clean and ready.\n\
           - `MAO_VERDICT: CHANGES_REQUESTED` — if high/medium issues must be fixed."
    )
}

fn format_findings_block(findings: &[MaoFinding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let items = findings
        .iter()
        .take(16)
        .map(|f| {
            format!(
                "- [{}] {} | {}",
                f.severity.to_ascii_uppercase(),
                f.location,
                f.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n\nStructured findings to fix:\n{items}")
}

fn build_rework_prompt(
    goal: &str,
    round: u8,
    max_rounds: u8,
    reviewer_name: &str,
    feedback: &str,
    findings: &[MaoFinding],
) -> String {
    let issues_block = format_findings_block(findings);
    format!(
        "[MAO Automated Rework · Round {round}/{max_rounds}]\n\n\
         Code Reviewer ({reviewer_name}) requested the following changes for task:\n{goal}\n\n\
         Review feedback:\n{feedback}{issues_block}\n\n\
         Please address every issue raised above, re-run verification/tests to confirm they pass, and summarize the fixes."
    )
}

pub fn continue_rework(s: &mut UiState, run_id: &str) {
    let extra_note = if s
        .orchestration
        .rework_note_run
        .as_deref()
        .is_none_or(|id| id == run_id)
    {
        let trimmed = s.orchestration.rework_note.text.trim().to_string();
        if !trimmed.is_empty() {
            s.orchestration.rework_note = TextEdit::new("");
            s.orchestration.rework_note_run = None;
            Some(trimmed)
        } else {
            None
        }
    } else {
        None
    };

    if run_id.starts_with("o_") {
        let computer = s.current_computer().to_string();
        if !computer.is_empty() && computer != "all" {
            let feedback = extra_note.map(|note| {
                let prior = s
                    .orchestration
                    .runs
                    .iter()
                    .find(|r| r.id == run_id)
                    .map(|r| r.review_summary.trim())
                    .filter(|summary| !summary.is_empty());
                match prior {
                    Some(prev) => format!("{prev}\n\nAdditional user guidance:\n{note}"),
                    None => note,
                }
            });
            let ticket = s.next_token;
            s.next_token += 1;
            s.send(Command::Orchestration {
                computer,
                ticket,
                request: crate::hub::OrchestrationRequest::Rework {
                    run: run_id.to_string(),
                    feedback,
                },
            });
            s.notify(
                ToastLevel::Info,
                tr(s.cfg.general.language, "MAO rework dispatched"),
                None,
            );
        }
        return;
    }
    let lang = s.cfg.general.language;
    let now = now_ms();
    let Some((coder_asid, prompt, echo, next_round, reviewer_name)) = (|| {
        let run = s.orchestration.runs.iter_mut().find(|r| r.id == run_id)?;
        let MaoPhase::ChangesRequested {
            round,
            ref feedback,
        } = run.phase
        else {
            return None;
        };
        let feedback = match extra_note.as_deref() {
            Some(note) if !feedback.trim().is_empty() => {
                format!("{feedback}\n\nAdditional user guidance:\n{note}")
            }
            Some(note) => note.to_string(),
            None => feedback.clone(),
        };
        let next_round = round.saturating_add(1);
        if next_round > run.max_rounds {
            run.max_rounds = next_round;
        }
        run.round = next_round;
        run.phase = MaoPhase::Coding { round: next_round };
        run.pending_review_diff = false;
        run.updated_ms = now;
        let coder_name = agents::name(&run.coder.agent, &s.agents);
        let reviewer_name = agents::name(&run.reviewer.agent, &s.agents);
        run.steps.push(MaoStep {
            kind: MaoStepKind::Rework,
            round: next_round,
            title: format!("{} #{next_round} · {coder_name}", tr_ref(lang, "Rework")),
            detail: concise_excerpt(&feedback, 220),
            status: "running",
            asid: Some(run.coder_asid.clone()),
            time_ms: now,
        });
        let prompt = build_rework_prompt(
            &run.goal,
            next_round,
            run.max_rounds,
            &reviewer_name,
            &feedback,
            &run.findings,
        );
        let echo = format!(
            "[MAO · Manual Rework Round {next_round}]\n\n{}{}",
            concise_excerpt(&feedback, 600),
            format_findings_block(&run.findings)
        );
        Some((
            run.coder_asid.clone(),
            prompt,
            echo,
            next_round,
            reviewer_name,
        ))
    })() else {
        return;
    };

    let token = s.next_token;
    s.next_token += 1;
    s.echo_sent(&coder_asid, token, echo, &[], None);
    s.send(Command::Compose(ComposeCommand::Submit {
        token,
        target: Target::Session(AgentSessionId(coder_asid)),
        deliver: Deliver::Prompt(prompt),
        attachments: Vec::new(),
        delivery: None,
        auto_approve: true,
    }));
    s.notify(
        ToastLevel::Info,
        tr(lang, "MAO rework dispatched"),
        Some(format!("{reviewer_name} → #{next_round}")),
    );
    s.sync_watch();
    persist_runs(s);
}

pub fn mark_approved(s: &mut UiState, run_id: &str) {
    if run_id.starts_with("o_") {
        let computer = s.current_computer().to_string();
        if !computer.is_empty() && computer != "all" {
            let ticket = s.next_token;
            s.next_token += 1;
            s.send(Command::Orchestration {
                computer,
                ticket,
                request: crate::hub::OrchestrationRequest::Approve(run_id.to_string()),
            });
        }
    }
    let lang = s.cfg.general.language;
    let now = now_ms();
    if let Some(run) = s.orchestration.runs.iter_mut().find(|r| r.id == run_id) {
        let round = run.round;
        run.phase = MaoPhase::Approved {
            round,
            summary: tr(lang, "Accepted manually."),
        };
        run.pending_review_diff = false;
        run.updated_ms = now;
        run.steps.push(MaoStep {
            kind: MaoStepKind::Verdict,
            round,
            title: tr(lang, "Accepted"),
            detail: tr(lang, "Marked approved manually."),
            status: "completed",
            asid: None,
            time_ms: now,
        });
        s.sync_watch();
        persist_runs(s);
    }
}

pub fn cancel_run(s: &mut UiState, run_id: &str) {
    if run_id.starts_with("o_") {
        let computer = s.current_computer().to_string();
        if !computer.is_empty() && computer != "all" {
            let ticket = s.next_token;
            s.next_token += 1;
            s.send(Command::Orchestration {
                computer,
                ticket,
                request: crate::hub::OrchestrationRequest::Cancel(run_id.to_string()),
            });
        }
    }
    let lang = s.cfg.general.language;
    let now = now_ms();
    let Some((coder, reviewer)) = (|| {
        let run = s.orchestration.runs.iter_mut().find(|r| r.id == run_id)?;
        run.phase = MaoPhase::Cancelled;
        run.pending_review_diff = false;
        run.updated_ms = now;
        for step in &mut run.steps {
            if step.status == "running" {
                step.status = "cancelled";
            }
        }
        run.steps.push(MaoStep {
            kind: MaoStepKind::System,
            round: run.round,
            title: tr(lang, "Stopped"),
            detail: tr(lang, "Orchestration stopped by user."),
            status: "cancelled",
            asid: None,
            time_ms: now,
        });
        Some((run.coder_asid.clone(), run.reviewer_asid.clone()))
    })() else {
        return;
    };

    if !coder.is_empty() && !coder.starts_with("new-") {
        s.send(Command::Interrupt(AgentSessionId(coder)));
    }
    if let Some(rev) = reviewer {
        if !rev.is_empty() && !rev.starts_with("new-") {
            s.send(Command::Interrupt(AgentSessionId(rev)));
        }
    }
    s.sync_watch();
    persist_runs(s);
}

// ---- Composer Bar -------------------------------------------------------------------------

pub fn composer_bar(s: &UiState, k: &Kit, layer: &mut Vec<El>, slot: Slot) -> El {
    let language = s.cfg.general.language;
    let enabled = is_slot_enabled(s, slot);
    let coder_id = active_coder_agent(s, slot);
    let asid = slot_asid(s, slot);
    let reviewer = effective_reviewer_for(s, &coder_id, asid);
    let max_rounds = effective_max_rounds(s, asid);

    let icon_color = if enabled {
        k.c.bg_accent
    } else {
        k.c.icon_muted
    };
    let text_accent = k.c.text_accent;
    let toggle = Button::new(tr_ref(language, "MAO"))
        .label_with(move |font, ink| {
            let tint = if enabled { text_accent } else { ink };
            row()
                .gap(5.0)
                .center_y()
                .child(icon(&glyphs::WORKFLOW, 12.0).color(icon_color))
                .child(text(tr_ref(language, "MAO")).font(font).color(tint))
        })
        .variant(if enabled {
            ButtonVariant::Neutral
        } else {
            ButtonVariant::GhostMuted
        })
        .control_size(ControlSize::Small)
        .help(tr(
            language,
            "Enable Coder ⇄ Reviewer intelligent orchestration",
        ))
        .on_press(move |s: &mut UiState| toggle_enabled(s, slot))
        .build(k)
        .key(match slot {
            Slot::Start => "mao-toggle-start",
            Slot::Session => "mao-toggle-session",
        })
        .radius(k.radius.lg)
        .border(
            1.0,
            if enabled {
                k.c.border_focus.alpha(0.65)
            } else {
                k.c.border_weak
            },
        );

    let mut bar = row()
        .key(match slot {
            Slot::Start => "mao-bar-start",
            Slot::Session => "mao-bar-session",
        })
        .gap(6.0)
        .center_y()
        .flex_wrap()
        .child(toggle);

    if enabled {
        let anchor = match slot {
            Slot::Start => MaoMenuAnchor::StartComposer,
            Slot::Session => MaoMenuAnchor::SessionComposer,
        };
        let reviewer_menu = reviewer_config_menu(
            s,
            k,
            layer,
            anchor,
            &coder_id,
            &reviewer,
            max_rounds,
            Placement::TopStart,
        );
        bar = bar.child(reviewer_menu);
    }

    bar
}

fn reviewer_summary_label(
    s: &UiState,
    reviewer: &MaoRole,
    max_rounds: u8,
    language: Language,
) -> String {
    let agent_name = s.agent_name(&reviewer.agent);
    let (model, display) = resolved_reviewer_model(s, reviewer);
    let machine_suffix = reviewer.machine.as_deref().map(|m| {
        let label = s
            .peers
            .iter()
            .find(|p| p.server_id == m || p.name == m)
            .map(|p| p.name.as_str())
            .unwrap_or(m);
        format!(" @ {label}")
    });
    let mut parts = vec![format!(
        "{}: {agent_name}{}",
        tr_ref(language, "Reviewer"),
        machine_suffix.as_deref().unwrap_or("")
    )];
    if let Some(m) = &model {
        parts.push(format_model_name(m, "Default", display));
        if let Some(v) = m.variant.as_deref() {
            parts.push(readable_catalog_id(v));
        }
    }
    parts.push(format!("{max_rounds}R"));
    parts.join(" · ")
}

#[allow(clippy::too_many_arguments)]
fn reviewer_config_menu(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    anchor: MaoMenuAnchor,
    coder_id: &str,
    reviewer: &MaoRole,
    max_rounds: u8,
    placement: Placement,
) -> El {
    let slot = anchor.slot();
    let language = s.cfg.general.language;
    let open = s.menu == Some(OpenMenu::MaoReviewer(anchor));
    let section = s.orchestration.menu_section.unwrap_or(MenuSection::Agent);
    let summary = reviewer_summary_label(s, reviewer, max_rounds, language);
    let rev_agent_id = reviewer.agent.clone();
    let dark = s.dark;
    let arrow = super::art::directional_arrow(
        s,
        k,
        11.0,
        if open {
            -std::f32::consts::FRAC_PI_2
        } else {
            std::f32::consts::FRAC_PI_2
        },
        k.c.icon_muted,
    );

    let trigger = Button::new(summary.clone())
        .label_with(move |font, ink| {
            row()
                .gap(6.0)
                .center_y()
                .child(agents::mark(&rev_agent_id, 13.0, dark))
                .child(text(summary).font(font).color(ink).truncate())
                .child(arrow)
        })
        .variant(ButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .expanded(open)
        .on_press(move |s: &mut UiState| {
            if open {
                s.menu = None;
            } else {
                let coder = active_coder_agent(s, slot);
                let asid = slot_asid(s, slot).map(str::to_string);
                let role = effective_reviewer_for(s, &coder, asid.as_deref());
                ensure_reviewer_catalog(s, &role.agent);
                s.orchestration.menu_section = Some(MenuSection::Agent);
                s.menu = Some(OpenMenu::MaoReviewer(anchor));
            }
        })
        .build(k)
        .key(anchor.pill_key())
        .radius(k.radius.lg)
        .border(
            1.0,
            if open {
                k.c.border_focus
            } else {
                k.c.border_weak
            },
        );

    let mut menu = Menu::new(anchor.menu_key())
        .placement(placement)
        .trigger(trigger)
        .open(open)
        .min_width(280.0)
        .on_close(|s: &mut UiState| s.menu = None);

    let coder_name = s.agent_name(coder_id);
    let rev_name = s.agent_name(&reviewer.agent);
    let current_asid = slot_asid(s, slot);
    let is_session_custom = has_session_config_override(s, current_asid);
    let scope_note = match slot {
        Slot::Session if is_session_custom => tr_ref(language, "Session custom"),
        Slot::Session => tr_ref(language, "Global default"),
        Slot::Start => tr_ref(language, "Global default"),
    };
    menu = menu.group_label(format!("{coder_name} ⇄ {rev_name} · {scope_note}"));

    // 1. Agent submenu
    let agent_entries: Vec<MenuEntry> = usable_agents(s)
        .into_iter()
        .map(|a| {
            let id = a.id.clone();
            let selected = id == reviewer.agent;
            MenuEntry::item(
                MenuItem::new(s.agent_name(&id))
                    .lead(agents::mark(&id, 14.0, dark))
                    .checked(selected)
                    .on_select(move |s: &mut UiState| set_reviewer_agent(s, anchor, id.clone())),
            )
        })
        .collect();
    menu = menu.item(
        MenuItem::new(tr_ref(language, "Reviewer agent"))
            .badge(rev_name)
            .submenu(agent_entries, section == MenuSection::Agent)
            .on_select(move |s: &mut UiState| {
                s.orchestration.menu_section = Some(MenuSection::Agent);
                s.menu = Some(OpenMenu::MaoReviewer(anchor));
            }),
    );

    // 2. Model submenu
    let (resolved_model, model_display) = resolved_reviewer_model(s, reviewer);
    let current_model_name = resolved_model
        .as_ref()
        .map(|m| format_model_name(m, "Default", model_display))
        .unwrap_or_else(|| tr(language, "Default"));
    let models = model_options(s, &reviewer.agent);
    let distinct_providers = models
        .iter()
        .map(|(p, _, _, _)| p.as_str())
        .collect::<HashSet<_>>()
        .len();
    let mut model_entries: Vec<MenuEntry> = if models.is_empty() {
        vec![MenuEntry::item(
            MenuItem::new(tr_ref(language, "Agent default model"))
                .checked(reviewer.model.is_none())
                .disabled(true),
        )]
    } else {
        let mut entries = Vec::new();
        let mut last_provider: Option<String> = None;
        for (provider, model_id, display, _) in models.into_iter().take(24) {
            if distinct_providers > 1 && last_provider.as_deref() != Some(provider.as_str()) {
                entries.push(MenuEntry::Label(provider.clone()));
                last_provider = Some(provider.clone());
            }
            let is_on = resolved_model
                .as_ref()
                .is_some_and(|cur| cur.provider_id == provider && cur.model_id == model_id);
            entries.push(MenuEntry::item(
                MenuItem::new(display)
                    .checked(is_on)
                    .on_select(move |s: &mut UiState| {
                        set_reviewer_model(s, anchor, provider.clone(), model_id.clone());
                    }),
            ));
        }
        entries
    };
    if reviewer.model.is_some() {
        model_entries.insert(
            0,
            MenuEntry::item(
                MenuItem::new(tr_ref(language, "Use agent default"))
                    .checked(false)
                    .on_select(move |s: &mut UiState| {
                        let coder = active_coder_agent(s, slot);
                        let asid = slot_asid(s, slot).map(str::to_string);
                        let cur = effective_reviewer_for(s, &coder, asid.as_deref());
                        let role = MaoRoleConfig {
                            agent: cur.agent,
                            model: None,
                            machine: cur.machine,
                        };
                        mutate_slot_config(s, slot, |target| match target {
                            SlotConfigTarget::Session(ov) => ov.reviewer = Some(role),
                            SlotConfigTarget::Global(global) => global.reviewer = Some(role),
                        });
                        s.menu = Some(OpenMenu::MaoReviewer(anchor));
                    }),
            ),
        );
    }
    menu = menu.item(
        MenuItem::new(tr_ref(language, "Model"))
            .badge(current_model_name)
            .submenu(model_entries, section == MenuSection::Model)
            .on_select(move |s: &mut UiState| {
                let coder = active_coder_agent(s, slot);
                let asid = slot_asid(s, slot).map(str::to_string);
                let role = effective_reviewer_for(s, &coder, asid.as_deref());
                ensure_reviewer_catalog(s, &role.agent);
                s.orchestration.menu_section = Some(MenuSection::Model);
                s.menu = Some(OpenMenu::MaoReviewer(anchor));
            }),
    );

    // 3. Effort submenu
    let variants = variants_for_model(s, &reviewer.agent, resolved_model.as_ref());
    let current_effort = resolved_model
        .as_ref()
        .and_then(|m| m.variant.as_deref())
        .map(readable_catalog_id)
        .unwrap_or_else(|| tr(language, "Default"));
    let effort_entries: Vec<MenuEntry> = if variants.is_empty() {
        vec![MenuEntry::item(
            MenuItem::new(tr_ref(language, "Default")).checked(true),
        )]
    } else {
        let cur_variant = resolved_model.as_ref().and_then(|m| m.variant.as_deref());
        let mut list = vec![MenuEntry::item(
            MenuItem::new(tr_ref(language, "Default"))
                .checked(cur_variant.is_none())
                .on_select(move |s: &mut UiState| set_reviewer_effort(s, anchor, None)),
        )];
        for v in variants {
            let is_on = cur_variant == Some(v.as_str());
            let label = readable_catalog_id(&v);
            list.push(MenuEntry::item(
                MenuItem::new(label)
                    .checked(is_on)
                    .on_select(move |s: &mut UiState| {
                        set_reviewer_effort(s, anchor, Some(v.clone()))
                    }),
            ));
        }
        list
    };
    menu = menu.item(
        MenuItem::new(tr_ref(language, "Reasoning effort"))
            .badge(current_effort)
            .submenu(effort_entries, section == MenuSection::Effort)
            .on_select(move |s: &mut UiState| {
                s.orchestration.menu_section = Some(MenuSection::Effort);
                s.menu = Some(OpenMenu::MaoReviewer(anchor));
            }),
    );

    // 4. Reviewer target computer (when fleet peers are linked)
    if s.peers.len() > 1 {
        let current_machine_label = reviewer
            .machine
            .as_deref()
            .map(|m| {
                s.peers
                    .iter()
                    .find(|p| p.server_id == m || p.name == m)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| m.to_string())
            })
            .unwrap_or_else(|| tr(language, "This computer"));
        let mut machine_entries = vec![MenuEntry::item(
            MenuItem::new(tr_ref(language, "This computer"))
                .checked(reviewer.machine.is_none())
                .on_select(move |s: &mut UiState| set_reviewer_machine(s, anchor, None)),
        )];
        for peer in &s.peers {
            if peer.current {
                continue;
            }
            let target_id = peer.name.clone();
            let is_on = reviewer
                .machine
                .as_deref()
                .is_some_and(|m| m == peer.name || m == peer.server_id);
            let badge = if peer.reachable {
                tr_ref(language, "Online")
            } else {
                tr_ref(language, "Offline")
            };
            machine_entries.push(MenuEntry::item(
                MenuItem::new(peer.name.clone())
                    .badge(badge)
                    .checked(is_on)
                    .disabled(!peer.reachable)
                    .on_select(move |s: &mut UiState| {
                        set_reviewer_machine(s, anchor, Some(target_id.clone()));
                    }),
            ));
        }
        menu = menu.item(
            MenuItem::new(tr_ref(language, "Computer"))
                .badge(current_machine_label)
                .submenu(machine_entries, section == MenuSection::Machine)
                .on_select(move |s: &mut UiState| {
                    s.orchestration.menu_section = Some(MenuSection::Machine);
                    s.menu = Some(OpenMenu::MaoReviewer(anchor));
                }),
        );
    }

    // 5. Max review rounds
    let round_entries: Vec<MenuEntry> = [1_u8, 2, 3, 5]
        .into_iter()
        .map(|n| {
            MenuEntry::item(
                MenuItem::new(format!("{n}"))
                    .description(tr(language, "Max Coder ⇄ Reviewer rounds"))
                    .checked(max_rounds == n)
                    .on_select(move |s: &mut UiState| set_max_rounds(s, anchor, n)),
            )
        })
        .collect();
    menu = menu.item(
        MenuItem::new(tr_ref(language, "Max review rounds"))
            .badge(format!("{max_rounds}"))
            .submenu(round_entries, section == MenuSection::Rounds)
            .on_select(move |s: &mut UiState| {
                s.orchestration.menu_section = Some(MenuSection::Rounds);
                s.menu = Some(OpenMenu::MaoReviewer(anchor));
            }),
    );

    let effective_failover = effective_auto_failover(s, current_asid);
    menu = menu
        .separator()
        .item(
            MenuItem::new(tr_ref(language, "Auto-failover on quota limit"))
                .checked(effective_failover)
                .on_select(move |s: &mut UiState| toggle_auto_failover(s, anchor)),
        )
        .item(
            MenuItem::new(tr_ref(language, "Agent & model pool"))
                .badge(if s.cfg.ui.mao.agent_pool.is_empty() {
                    tr(language, "Not configured")
                } else {
                    format!("{}", s.cfg.ui.mao.agent_pool.len())
                })
                .on_select(|s: &mut UiState| {
                    s.menu = None;
                    s.settings_tab = super::SettingsTab::Agents;
                    s.go(Mode::Settings);
                }),
        );
    if slot == Slot::Session && is_session_custom {
        menu = menu.item(
            MenuItem::new(tr_ref(language, "Reset to global default"))
                .on_select(move |s: &mut UiState| reset_session_mao_config(s, anchor)),
        );
    }
    menu = menu.separator().item(
        MenuItem::new(tr_ref(language, "Open MAO dashboard"))
            .icon(&glyphs::WORKFLOW)
            .badge(if s.orchestration.runs.is_empty() {
                String::new()
            } else {
                s.orchestration.runs.len().to_string()
            })
            .on_select(|s: &mut UiState| {
                s.menu = None;
                s.go(Mode::Mao);
            }),
    );

    menu.build(k, layer)
}

// ---- Per-Agent Live Inspection & Quality Gates --------------------------------------------

struct AgentLiveSnapshot {
    status_label: String,
    dot_color: Color,
    activity_line: String,
    waiting_attention: bool,
    is_busy: bool,
}

fn summarize_tool_headline(call: &muqun_gateway::agents::ToolCall) -> String {
    call.title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .map(|t| concise_excerpt(t, 44))
        .unwrap_or_else(|| readable_catalog_id(&call.name))
}

fn inspect_agent_live(
    s: &UiState,
    k: &Kit,
    asid: Option<&str>,
    is_active_role: bool,
    language: Language,
) -> AgentLiveSnapshot {
    let Some(id) = asid.filter(|id| !id.is_empty() && !id.starts_with("new-")) else {
        return AgentLiveSnapshot {
            status_label: tr(language, "Pending start"),
            dot_color: k.c.text_faint,
            activity_line: tr(language, "Waiting for session assignment…"),
            waiting_attention: false,
            is_busy: false,
        };
    };

    let perms = s.waiting_perms.get(id).map_or(0, Vec::len);
    let forms = s.waiting_forms.get(id).map_or(0, Vec::len);
    let waiting_attention = perms + forms > 0;
    let session_status = s.session_by_id(id).map(|info| info.status);

    let mut tool_count = 0usize;
    let mut running_headline: Option<String> = None;
    let mut last_headline: Option<String> = None;

    if let Some(tl) = s.timelines.get(id) {
        for item in &tl.items {
            match &item.part {
                AgentPart::Tool(call) => {
                    tool_count += 1;
                    let head = summarize_tool_headline(call);
                    if matches!(
                        call.state,
                        ToolCallStatus::Pending
                            | ToolCallStatus::Streaming
                            | ToolCallStatus::Running
                    ) {
                        running_headline = Some(head);
                    } else {
                        last_headline = Some(head);
                    }
                }
                AgentPart::Shell {
                    command, status, ..
                } => {
                    tool_count += 1;
                    let head = concise_excerpt(command, 44);
                    if status == "running" {
                        running_headline = Some(head);
                    } else {
                        last_headline = Some(head);
                    }
                }
                _ => {}
            }
        }
    }

    let is_busy = matches!(
        session_status,
        Some(AgentSessionStatus::Busy | AgentSessionStatus::Retry)
    ) || is_active_role
        || running_headline.is_some();

    let (status_label, dot_color) = if waiting_attention {
        (tr(language, "Waiting for approval"), k.c.warning.icon)
    } else if matches!(session_status, Some(AgentSessionStatus::Failed)) {
        (tr(language, "Failed"), k.c.danger.icon)
    } else if matches!(session_status, Some(AgentSessionStatus::Interrupted)) {
        (tr(language, "Stopped"), k.c.text_faint)
    } else if is_busy {
        (tr(language, "Busy · Running"), k.c.text_accent)
    } else {
        (tr(language, "Idle · Done"), k.c.success.icon)
    };

    let activity_line = if waiting_attention {
        tr(language, "Waiting for approval")
    } else if let Some(running) = running_headline {
        format!("{}: {running}", tr_ref(language, "Active tool"))
    } else if tool_count > 0 {
        match last_headline {
            Some(last) => format!("{tool_count} {} · {last}", tr_ref(language, "tool calls")),
            None => format!("{tool_count} {}", tr_ref(language, "tool calls")),
        }
    } else if is_busy {
        tr(language, "Working…")
    } else {
        tr(language, "Idle · Done")
    };

    AgentLiveSnapshot {
        status_label,
        dot_color,
        activity_line,
        waiting_attention,
        is_busy,
    }
}

fn diff_totals(files: &[MaoDeliverableFile]) -> (usize, usize) {
    files.iter().fold((0usize, 0usize), |(a, d), f| {
        (a.saturating_add(f.additions), d.saturating_add(f.deletions))
    })
}

fn gate_status_colors(k: &Kit, status: &'static str) -> (Color, Color, Color) {
    match status {
        "completed" => (
            k.c.success.icon.alpha(0.12),
            k.c.success.icon.alpha(0.45),
            k.c.success.fg,
        ),
        "running" => (
            k.c.bg_accent.alpha(0.14),
            k.c.bg_accent.alpha(0.55),
            k.c.text_base,
        ),
        "warning" => (
            k.c.warning.icon.alpha(0.14),
            k.c.warning.icon.alpha(0.45),
            k.c.warning.fg,
        ),
        "failed" => (
            k.c.danger.icon.alpha(0.14),
            k.c.danger.icon.alpha(0.45),
            k.c.danger.fg,
        ),
        _ => (k.c.layer_02, k.c.border_weak, k.c.text_muted),
    }
}

struct QualityGate {
    title: &'static str,
    detail: String,
    status: &'static str,
}

fn quality_gates_for_run(run: &MaoRun, language: Language) -> [QualityGate; 4] {
    let coding_gate = match &run.phase {
        MaoPhase::Coding { round } => QualityGate {
            title: tr_ref(language, "Gate: Coding"),
            detail: format!("{} #{round}", tr_ref(language, "Coding")),
            status: "running",
        },
        MaoPhase::Failed { .. } => QualityGate {
            title: tr_ref(language, "Gate: Coding"),
            detail: tr(language, "Failed"),
            status: "failed",
        },
        MaoPhase::Cancelled => QualityGate {
            title: tr_ref(language, "Gate: Coding"),
            detail: tr(language, "Stopped"),
            status: "idle",
        },
        _ => QualityGate {
            title: tr_ref(language, "Gate: Coding"),
            detail: tr(language, "Completed"),
            status: "completed",
        },
    };

    let (add_total, del_total) = diff_totals(&run.files);
    let diff_gate = if !run.files.is_empty() {
        QualityGate {
            title: tr_ref(language, "Gate: Diff"),
            detail: format!(
                "{} {} (+{add_total} -{del_total})",
                run.files.len(),
                tr_ref(language, "changed files")
            ),
            status: "completed",
        }
    } else if matches!(
        run.phase,
        MaoPhase::Reviewing { .. } | MaoPhase::Approved { .. }
    ) {
        QualityGate {
            title: tr_ref(language, "Gate: Diff"),
            detail: tr(language, "Completed"),
            status: "completed",
        }
    } else {
        QualityGate {
            title: tr_ref(language, "Gate: Diff"),
            detail: tr(language, "Pending review"),
            status: "idle",
        }
    };

    let review_gate = match &run.phase {
        MaoPhase::Reviewing { round } => QualityGate {
            title: tr_ref(language, "Gate: Review"),
            detail: format!(
                "{} #{round}/{}",
                tr_ref(language, "Reviewing"),
                run.max_rounds
            ),
            status: "running",
        },
        MaoPhase::Approved { round, .. } => QualityGate {
            title: tr_ref(language, "Gate: Review"),
            detail: format!("{} #{round}", tr_ref(language, "Approved")),
            status: "completed",
        },
        MaoPhase::ChangesRequested { round, .. } => QualityGate {
            title: tr_ref(language, "Gate: Review"),
            detail: format!("{} #{round}", tr_ref(language, "Needs fix")),
            status: "warning",
        },
        MaoPhase::Coding { round } if *round > 1 => QualityGate {
            title: tr_ref(language, "Gate: Review"),
            detail: format!("{} #{}", tr_ref(language, "Rework"), round),
            status: "warning",
        },
        _ => QualityGate {
            title: tr_ref(language, "Gate: Review"),
            detail: tr(language, "Pending review"),
            status: "idle",
        },
    };

    let quality_gate = match &run.phase {
        MaoPhase::Approved { .. } => QualityGate {
            title: tr_ref(language, "Gate: Quality"),
            detail: tr(language, "Quality passed"),
            status: "completed",
        },
        MaoPhase::ChangesRequested { .. } => QualityGate {
            title: tr_ref(language, "Gate: Quality"),
            detail: tr(language, "Needs fix"),
            status: "warning",
        },
        MaoPhase::Failed { .. } => QualityGate {
            title: tr_ref(language, "Gate: Quality"),
            detail: tr(language, "Failed"),
            status: "failed",
        },
        MaoPhase::Cancelled => QualityGate {
            title: tr_ref(language, "Gate: Quality"),
            detail: tr(language, "Stopped"),
            status: "idle",
        },
        MaoPhase::Coding { .. } | MaoPhase::Reviewing { .. } => QualityGate {
            title: tr_ref(language, "Gate: Quality"),
            detail: tr(language, "Pending review"),
            status: "idle",
        },
    };

    [coding_gate, diff_gate, review_gate, quality_gate]
}

// ---- Session Pipeline Banner --------------------------------------------------------------

pub fn session_banner(s: &UiState, k: &Kit) -> Option<El> {
    let current_asid = s.selected.as_deref()?;
    let run = s.orchestration.run_for_session(current_asid)?;
    let language = s.cfg.general.language;
    let is_reviewer_view = run.reviewer_asid.as_deref() == Some(current_asid);
    let accent = phase_color(k, &run.phase);

    let coder_name = s.agent_name(&run.coder.agent);
    let reviewer_name = s.agent_name(&run.reviewer.agent);
    let coder_live = inspect_agent_live(
        s,
        k,
        Some(run.coder_asid.as_str()),
        matches!(run.phase, MaoPhase::Coding { .. }),
        language,
    );
    let rev_live = inspect_agent_live(
        s,
        k,
        run.reviewer_asid.as_deref(),
        matches!(run.phase, MaoPhase::Reviewing { .. }),
        language,
    );

    let coder_clickable = !run.coder_asid.starts_with("new-") && is_reviewer_view;

    let coder_stage = stage_chip(
        k,
        format!("1. {} · {coder_name}", tr_ref(language, "Coding")),
        &run.coder.agent,
        coder_live.dot_color,
        matches!(run.phase, MaoPhase::Coding { .. }),
        matches!(
            run.phase,
            MaoPhase::Reviewing { .. }
                | MaoPhase::Approved { .. }
                | MaoPhase::ChangesRequested { .. }
        ),
        coder_clickable.then(|| run.coder_asid.clone()),
        s.dark,
    );

    let review_label = format!(
        "2. {} {}/{} · {reviewer_name}",
        tr_ref(language, "Review"),
        run.round,
        run.max_rounds
    );
    let review_stage = stage_chip(
        k,
        review_label,
        &run.reviewer.agent,
        rev_live.dot_color,
        matches!(run.phase, MaoPhase::Reviewing { .. }),
        matches!(run.phase, MaoPhase::Approved { .. }),
        run.reviewer_asid
            .as_ref()
            .filter(|id| !id.starts_with("new-"))
            .map(|rev| {
                if !is_reviewer_view
                    && !run.coder_asid.starts_with("new-")
                    && run.coder_asid != *rev
                {
                    let coder_parent = run.coder_asid.clone();
                    let rev_child = rev.clone();
                    // Clicking Stage 2 while watching Coder opens/focuses the Reviewer window directly.
                    format!("detail:{coder_parent}:{rev_child}")
                } else {
                    rev.clone()
                }
            }),
        s.dark,
    );

    let verdict_label = match &run.phase {
        MaoPhase::Approved { .. } => format!("3. {}", tr_ref(language, "Quality passed")),
        MaoPhase::ChangesRequested { .. } => format!("3. {}", tr_ref(language, "Needs fix")),
        MaoPhase::Failed { .. } => format!("3. {}", tr_ref(language, "Failed")),
        MaoPhase::Cancelled => format!("3. {}", tr_ref(language, "Stopped")),
        MaoPhase::Coding { .. } | MaoPhase::Reviewing { .. } => {
            format!("3. {}", tr_ref(language, "Pending review"))
        }
    };
    let verdict_status = match &run.phase {
        MaoPhase::Approved { .. } => "completed",
        MaoPhase::ChangesRequested { .. } => "warning",
        MaoPhase::Failed { .. } => "failed",
        _ => "idle",
    };
    let (v_fill, v_border, v_ink) = gate_status_colors(k, verdict_status);
    let verdict_chip = row()
        .h(24.0)
        .px(8.0)
        .center_y()
        .gap(5.0)
        .radius(k.radius.md)
        .fill(v_fill)
        .border(1.0, v_border)
        .child(text(verdict_label).font(k.ty.small_medium).color(v_ink));

    let mut left_actions = row().gap(6.0).center_y().flex_wrap();
    if rev_live.waiting_attention {
        if let Some(ref rev_id) = run.reviewer_asid {
            let target = rev_id.clone();
            left_actions = left_actions.child(
                Button::new(format!(
                    "{} · {reviewer_name}",
                    tr_ref(language, "Waiting for approval")
                ))
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.open_session(&target))
                .build(k)
                .key("mao-banner-attn-rev")
                .border(1.0, k.c.warning.icon.alpha(0.65)),
            );
        }
    } else if coder_live.waiting_attention
        && is_reviewer_view
        && !run.coder_asid.starts_with("new-")
    {
        let target = run.coder_asid.clone();
        left_actions = left_actions.child(
            Button::new(format!(
                "{} · {coder_name}",
                tr_ref(language, "Waiting for approval")
            ))
            .variant(ButtonVariant::Neutral)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| s.open_session(&target))
            .build(k)
            .key("mao-banner-attn-coder")
            .border(1.0, k.c.warning.icon.alpha(0.65)),
        );
    }

    if !run.files.is_empty() && !run.coder_asid.starts_with("new-") {
        let (adds, dels) = diff_totals(&run.files);
        let diff_target = run.coder_asid.clone();
        let files_count = run.files.len();
        left_actions = left_actions.child(
            row()
                .key("mao-banner-files-chip")
                .h(24.0)
                .px(8.0)
                .gap(5.0)
                .center_y()
                .radius(k.radius.md)
                .fill(k.c.layer_02)
                .border(1.0, k.c.border_weak)
                .on_press(move |s: &mut UiState| {
                    if s.selected.as_deref() != Some(diff_target.as_str()) {
                        s.open_session(&diff_target);
                    }
                    s.show_diff = true;
                    s.send(Command::RefreshDiff(AgentSessionId(diff_target.clone())));
                })
                .child(
                    text(format!(
                        "{files_count} {}",
                        tr_ref(language, "changed files")
                    ))
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted),
                )
                .child(
                    text(format!("+{adds}"))
                        .font(k.ty.small_medium)
                        .color(k.c.success.fg),
                )
                .child(
                    text(format!("-{dels}"))
                        .font(k.ty.small_medium)
                        .color(k.c.danger.fg),
                ),
        );
    }

    if is_reviewer_view && !run.coder_asid.starts_with("new-") {
        let target = run.coder_asid.clone();
        left_actions = left_actions.child(
            Button::new(tr_ref(language, "Switch to coder"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.open_session(&target))
                .build(k)
                .key("mao-jump-coder"),
        );
    } else if let Some(ref rev_asid) = run.reviewer_asid {
        if !rev_asid.starts_with("new-") {
            let coder_parent = run.coder_asid.clone();
            let rev_target = rev_asid.clone();
            left_actions = left_actions.child(
                Button::new(tr_ref(language, "Switch to reviewer"))
                    .variant(ButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        if !coder_parent.starts_with("new-") && coder_parent != rev_target {
                            s.open_detail(&coder_parent, &rev_target);
                        } else {
                            s.open_session(&rev_target);
                        }
                    })
                    .build(k)
                    .key("mao-jump-reviewer"),
            );
        }
    }

    let toggle_id = run.id.clone();
    let expanded = run.expanded;
    left_actions = left_actions
        .child(
            Button::new(if expanded {
                tr_ref(language, "Hide details")
            } else {
                tr_ref(language, "Details")
            })
            .variant(ButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| {
                if let Some(r) = s.orchestration.runs.iter_mut().find(|r| r.id == toggle_id) {
                    r.expanded = !r.expanded;
                }
            })
            .build(k)
            .key("mao-banner-expand"),
        )
        .child(
            Button::new(tr_ref(language, "MAO window"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.open_plugin_drawer("mao"))
                .build(k)
                .key("mao-banner-window"),
        );

    let mut right_actions = row().gap(6.0).center_y();
    match &run.phase {
        MaoPhase::Coding { .. } | MaoPhase::Reviewing { .. } => {
            let id = run.id.clone();
            right_actions = right_actions.child(
                Button::new(tr_ref(language, "Stop"))
                    .variant(ButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| cancel_run(s, &id))
                    .build(k)
                    .key("mao-banner-stop"),
            );
        }
        MaoPhase::ChangesRequested { .. } => {
            let id_rework = run.id.clone();
            let id_approve = run.id.clone();
            right_actions = right_actions
                .child(
                    Button::new(tr_ref(language, "Mark approved"))
                        .variant(ButtonVariant::GhostMuted)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| mark_approved(s, &id_approve))
                        .build(k)
                        .key("mao-banner-approve"),
                )
                .child(
                    Button::new(tr_ref(language, "Continue rework"))
                        .variant(ButtonVariant::Submit)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| continue_rework(s, &id_rework))
                        .build(k)
                        .key("mao-banner-rework"),
                );
        }
        _ => {}
    }

    // Single compact header bar: pipeline stepper on the left, actions on the right.
    let mut header_bar = row()
        .w_full()
        .gap(6.0)
        .center_y()
        .flex_wrap()
        .child(icon(&glyphs::WORKFLOW, 14.0).color(accent))
        .child(coder_stage)
        .child(text("›").font(k.ty.small_medium).color(k.c.text_faint))
        .child(review_stage)
        .child(text("›").font(k.ty.small_medium).color(k.c.text_faint))
        .child(verdict_chip)
        .child(el().grow(1.0).min_w(8.0))
        .child(left_actions);
    if matches!(
        run.phase,
        MaoPhase::Coding { .. } | MaoPhase::Reviewing { .. } | MaoPhase::ChangesRequested { .. }
    ) {
        header_bar = header_bar.child(right_actions);
    }

    let active_activity = clean_one_line(&if matches!(run.phase, MaoPhase::Reviewing { .. }) {
        format!(
            "{reviewer_name}: {} · {}",
            rev_live.status_label, rev_live.activity_line
        )
    } else if matches!(run.phase, MaoPhase::Coding { .. }) {
        format!(
            "{coder_name}: {} · {}",
            coder_live.status_label, coder_live.activity_line
        )
    } else {
        phase_summary_line(run, &coder_name, &reviewer_name, language)
    });

    let rework_note_row = matches!(run.phase, MaoPhase::ChangesRequested { .. }).then(|| {
        let note_run_edit = run.id.clone();
        let note_run_submit = run.id.clone();
        let placeholder = tr_ref(language, "Optional extra guidance for rework…");
        row().w_full().pt(2.0).child(
            TextInput::new(&s.orchestration.rework_note)
                .placeholder(placeholder)
                .width(None)
                .on_edit(move |s: &mut UiState| {
                    s.orchestration.rework_note_run = Some(note_run_edit.clone());
                    &mut s.orchestration.rework_note
                })
                .on_submit(move |s: &mut UiState, _: &mut Tx| {
                    s.orchestration.rework_note_run = Some(note_run_submit.clone());
                    continue_rework(s, &note_run_submit);
                })
                .build(k)
                .key("mao-banner-rework-note")
                .w_full(),
        )
    });

    let mut inner = col()
        .w_full()
        .px(14.0)
        .py(8.0)
        .gap(6.0)
        .fill(k.c.layer_01)
        .child(header_bar)
        .children((!active_activity.is_empty()).then(|| {
            text(active_activity.clone())
                .font(k.ty.small)
                .color(if matches!(run.phase, MaoPhase::Failed { .. }) {
                    k.c.danger.fg
                } else {
                    k.c.text_muted
                })
                .single_line()
                .truncate()
        }))
        .children(rework_note_row);

    if run.expanded {
        let mut drawer_body = col().w_full().gap(8.0).pt(6.0);
        let coder_clean = clean_summary_excerpt(&run.coder_summary, 220);
        let review_clean = clean_summary_excerpt(&run.review_summary, 220);
        let show_review_card = !review_clean.is_empty() && review_clean != active_activity;
        if !coder_clean.is_empty() || show_review_card {
            let mut summary_row = row().w_full().gap(8.0).flex_wrap();
            if !coder_clean.is_empty() {
                summary_row = summary_row.child(
                    col()
                        .w(0.0)
                        .grow(1.0)
                        .min_w(220.0)
                        .px(10.0)
                        .py(8.0)
                        .gap(4.0)
                        .radius(k.radius.md)
                        .fill(k.c.layer_02)
                        .border(1.0, k.c.border_weak)
                        .child(
                            text(tr_ref(language, "Coder summary"))
                                .font(k.ty.small_medium)
                                .color(k.c.text_faint),
                        )
                        .child(
                            text(coder_clean)
                                .font(k.ty.small)
                                .color(k.c.text_base)
                                .wrap()
                                .w_full(),
                        ),
                );
            }
            if show_review_card {
                summary_row = summary_row.child(
                    col()
                        .w(0.0)
                        .grow(1.0)
                        .min_w(220.0)
                        .px(10.0)
                        .py(8.0)
                        .gap(4.0)
                        .radius(k.radius.md)
                        .fill(k.c.layer_02)
                        .border(1.0, k.c.border_weak)
                        .child(
                            text(tr_ref(language, "Review report"))
                                .font(k.ty.small_medium)
                                .color(k.c.text_faint),
                        )
                        .child(
                            text(review_clean)
                                .font(k.ty.small)
                                .color(k.c.text_base)
                                .wrap()
                                .w_full(),
                        ),
                );
            }
            drawer_body = drawer_body.child(summary_row);
        }
        if !run.steps.is_empty() {
            let mut steps_box = col().w_full().gap(4.0);
            for (idx, step) in run.steps.iter().rev().take(4).rev().enumerate() {
                steps_box = steps_box.child(step_row(k, idx, step));
            }
            drawer_body = drawer_body.child(steps_box);
        }
        inner = inner
            .child(
                el().w_full()
                    .h(1.0)
                    .fill(k.c.border_weak.alpha(0.55))
                    .a11y_hidden(),
            )
            .child(
                col()
                    .key("mao-banner-drawer-scroll")
                    .w_full()
                    .max_h(220.0)
                    .child(drawer_body)
                    .scroll(rune::ScrollSpec {
                        stick_bottom: false,
                    })
                    .scroll_bar(k),
            );
    }

    Some(
        col()
            .key(format!("mao-banner-{}", run.id))
            .w_full()
            .child(inner)
            .child(el().w_full().h(1.0).fill(k.c.border_weak).a11y_hidden()),
    )
}

#[allow(clippy::too_many_arguments)]
fn stage_chip(
    k: &Kit,
    label: String,
    agent_id: &str,
    dot_color: Color,
    active: bool,
    done: bool,
    jump_to: Option<String>,
    dark: bool,
) -> El {
    let fill = if active {
        k.c.bg_accent.alpha(0.14)
    } else if done {
        k.c.success.icon.alpha(0.12)
    } else {
        k.c.layer_02
    };
    let border = if active {
        k.c.bg_accent.alpha(0.55)
    } else if done {
        k.c.success.icon.alpha(0.4)
    } else {
        k.c.border_weak
    };
    let ink = if active {
        k.c.text_base
    } else if done {
        k.c.success.fg
    } else {
        k.c.text_muted
    };
    let mut chip = row()
        .h(24.0)
        .px(8.0)
        .center_y()
        .gap(5.0)
        .radius(k.radius.md)
        .fill(fill)
        .border(1.0, border)
        .child(el().size(6.0, 6.0).radius(3.0).fill(dot_color))
        .child(agents::mark(agent_id, 12.0, dark))
        .child(text(label).font(k.ty.small_medium).color(ink).single_line());
    if let Some(target) = jump_to {
        chip = chip.on_press(move |s: &mut UiState| {
            if let Some(rest) = target.strip_prefix("detail:") {
                if let Some((parent, child)) = rest.split_once(':') {
                    s.open_detail(parent, child);
                    return;
                }
            }
            s.open_session(&target);
        });
    }
    chip
}

fn phase_summary_line(
    run: &MaoRun,
    coder_name: &str,
    reviewer_name: &str,
    language: Language,
) -> String {
    match &run.phase {
        MaoPhase::Coding { round } if *round == 1 => format!(
            "{} · {coder_name} {}",
            tr_ref(language, "Phase 1"),
            tr_ref(
                language,
                "is implementing the task; code review starts automatically once idle."
            )
        ),
        MaoPhase::Coding { round } => format!(
            "{} #{round}/{} · {coder_name} {}",
            tr_ref(language, "Rework"),
            run.max_rounds,
            tr_ref(language, "is fixing issues raised by code review.")
        ),
        MaoPhase::Reviewing { round } => format!(
            "{} #{round}/{} · {reviewer_name} {}",
            tr_ref(language, "Code review"),
            run.max_rounds,
            tr_ref(language, "is inspecting changed files and diff.")
        ),
        MaoPhase::Approved { round, summary } => {
            if summary.is_empty() {
                format!(
                    "{reviewer_name} · #{round} {}",
                    tr_ref(language, "approved the changes.")
                )
            } else {
                format!("{reviewer_name} · #{round}: {summary}")
            }
        }
        MaoPhase::ChangesRequested { round, feedback } => {
            format!("{reviewer_name} · #{round}: {feedback}")
        }
        MaoPhase::Failed { error } => error.clone(),
        MaoPhase::Cancelled => tr(language, "MAO orchestration stopped."),
    }
}

fn clean_one_line(raw: &str) -> String {
    let mut parts = Vec::new();
    for line in raw.lines() {
        let stripped = line
            .trim()
            .trim_start_matches('#')
            .trim_start_matches('-')
            .trim_start_matches('*')
            .trim();
        if stripped.is_empty() {
            continue;
        }
        parts.push(stripped.replace("**", "").replace('`', ""));
    }
    parts
        .join(" · ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_summary_excerpt(raw: &str, max_chars: usize) -> String {
    let cleaned = clean_one_line(raw);
    concise_excerpt(&cleaned, max_chars)
}

fn step_row(k: &Kit, idx: usize, step: &MaoStep) -> El {
    let dot_color = match step.status {
        "running" => k.c.text_accent,
        "completed" => k.c.success.icon,
        "warning" => k.c.warning.icon,
        "failed" => k.c.danger.icon,
        _ => k.c.text_faint,
    };
    let clean_detail = clean_one_line(&step.detail);
    let mut item = row()
        .key(format!("mao-step-{idx}-{}", step.round))
        .w_full()
        .h(28.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_weak.alpha(0.45))
        .child(el().size(6.0, 6.0).radius(3.0).fill(dot_color))
        .child(
            text(step.title.clone())
                .font(k.ty.small_medium)
                .color(k.c.text_base)
                .single_line(),
        )
        .child(
            text(clean_detail)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .single_line()
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        );
    if let Some(asid) = &step.asid {
        if !asid.starts_with("new-") {
            let target = asid.clone();
            item = item.on_press(move |s: &mut UiState| s.open_session(&target));
        }
    }
    item
}

fn phase_color(k: &Kit, phase: &MaoPhase) -> Color {
    match phase {
        MaoPhase::Coding { .. } => k.c.text_accent,
        MaoPhase::Reviewing { .. } => k.c.info.icon,
        MaoPhase::Approved { .. } => k.c.success.icon,
        MaoPhase::ChangesRequested { .. } => k.c.warning.icon,
        MaoPhase::Failed { .. } => k.c.danger.icon,
        MaoPhase::Cancelled => k.c.text_faint,
    }
}

fn phase_badge(k: &Kit, phase: &MaoPhase, language: Language) -> El {
    let (label, variant) = match phase {
        MaoPhase::Coding { round } => (
            format!("{} #{round}", tr_ref(language, "Coding")),
            BadgeVariant::Accent,
        ),
        MaoPhase::Reviewing { round } => (
            format!("{} #{round}", tr_ref(language, "Reviewing")),
            BadgeVariant::Neutral,
        ),
        MaoPhase::Approved { .. } => (tr(language, "Approved"), BadgeVariant::Neutral),
        MaoPhase::ChangesRequested { .. } => {
            (tr(language, "Changes requested"), BadgeVariant::Neutral)
        }
        MaoPhase::Failed { .. } => (tr(language, "Failed"), BadgeVariant::Subtle),
        MaoPhase::Cancelled => (tr(language, "Stopped"), BadgeVariant::Subtle),
    };
    Badge::new(label).variant(variant).build(k)
}

// ---- Dashboard Page (Mode::Mao) -----------------------------------------------------------

pub fn page_view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let card_w = rune::memo::layout_with("main", |r| r[2])
        .unwrap_or(1080.0)
        .min(MAX_W);
    let stacked = card_w < STACK_AT;

    let list: Vec<&MaoRun> = s
        .orchestration
        .runs
        .iter()
        .rev()
        .filter(|r| s.orchestration.filter.keeps(r))
        .collect();
    let selected = s
        .orchestration
        .selected_run
        .as_deref()
        .and_then(|id| list.iter().copied().find(|r| r.id == id))
        .or_else(|| list.first().copied());

    let mut body = row().key("mao-body").w_full().grow(1.0).min_h(0.0);
    if s.orchestration.runs.is_empty() {
        body = body.child(page_empty(s, k));
    } else {
        body = body
            .child(page_list(s, k, &list, selected.map(|r| r.id.as_str())))
            .child(
                el().key("mao-rule")
                    .w(1.0)
                    .h_full()
                    .fill(k.c.border_weak)
                    .a11y_hidden(),
            )
            .child(match selected {
                Some(run) => page_detail(s, k, run),
                None => col()
                    .key("mao-detail-empty")
                    .w(0.0)
                    .grow(0.58)
                    .min_w(0.0)
                    .h_full()
                    .center()
                    .child(
                        text(tr_ref(language, "No matching MAO run"))
                            .font(k.ty.small)
                            .color(k.c.text_faint),
                    ),
            });
    }

    let page = col()
        .key("mao")
        .max_w(MAX_W)
        .h_full()
        .min_h(0.0)
        .child(page_head(s, k, layer, stacked))
        .children((!s.orchestration.runs.is_empty()).then(|| page_filters(s, k)))
        .child(body);

    row()
        .key("mao-centre")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(el().grow(1.0))
        .child(page.w_full())
        .child(el().grow(1.0))
}

fn page_head(s: &UiState, k: &Kit, layer: &mut Vec<El>, stacked: bool) -> El {
    let language = s.cfg.general.language;
    let enabled = s.cfg.ui.mao.enabled;
    let coder_id = s.start_agent.clone();
    let reviewer = effective_reviewer(s, &coder_id);
    let max_rounds = s.cfg.ui.mao.max_rounds.clamp(1, 5);
    let reviewer_menu = reviewer_config_menu(
        s,
        k,
        layer,
        MaoMenuAnchor::Page,
        &coder_id,
        &reviewer,
        max_rounds,
        Placement::BottomStart,
    );

    let controls = row()
        .gap(8.0)
        .center_y()
        .flex_wrap()
        .child(
            Button::new(if enabled {
                tr_ref(language, "MAO enabled")
            } else {
                tr_ref(language, "Enable MAO")
            })
            .variant(if enabled {
                ButtonVariant::Submit
            } else {
                ButtonVariant::Neutral
            })
            .control_size(ControlSize::Small)
            .on_press(|s: &mut UiState| toggle_enabled(s, Slot::Start))
            .build(k)
            .key("mao-page-toggle"),
        )
        .child(
            Button::new(tr_ref(language, "New MAO task"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| {
                    if !s.cfg.ui.mao.enabled {
                        toggle_enabled(s, Slot::Start);
                    }
                    s.go(Mode::Home);
                })
                .build(k)
                .key("mao-page-new"),
        )
        .child(
            Button::new(tr_ref(language, "Sync"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(request_gateway_sync)
                .build(k)
                .key("mao-page-sync"),
        );

    let line = if stacked { col() } else { row().center_y() };
    line.key("mao-head")
        .w_full()
        .px(28.0)
        .pt(26.0)
        .pb(14.0)
        .gap(if stacked { 12.0 } else { 24.0 })
        .child(
            col()
                .key("mao-title")
                .grow(if stacked { 0.0 } else { 1.0 })
                .shrink()
                .min_w(0.0)
                .gap(4.0)
                .child(
                    row()
                        .w_full()
                        .gap(10.0)
                        .center_y()
                        .flex_wrap()
                        .child(
                            text(tr_ref(language, "MAO · Orchestration"))
                                .key("page-title")
                                .font(TextStyle {
                                    size: 22.0,
                                    line_height: 28.0,
                                    ..k.ty.ui_medium
                                })
                                .color(k.c.text_base),
                        )
                        .child(reviewer_menu),
                )
                .child(
                    text(tr_ref(
                        language,
                        "Automated Coder ⇄ Reviewer delivery & code-review loop.",
                    ))
                    .key("page-subtitle")
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .wrap()
                    .w_full(),
                ),
        )
        .child(controls)
}

fn page_filters(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let labels: Vec<String> = Filter::ALL
        .iter()
        .map(|f| {
            let n = s.orchestration.runs.iter().filter(|r| f.keeps(r)).count();
            if *f == Filter::All || n == 0 {
                f.word(language).to_string()
            } else {
                format!("{} {n}", f.word(language))
            }
        })
        .collect();
    let at = Filter::ALL
        .iter()
        .position(|f| *f == s.orchestration.filter);

    row().key("mao-filters").w_full().px(28.0).pb(12.0).child(
        SegmentedControl::new(&labels, at)
            .fit()
            .label(tr_ref(language, "State"))
            .on_pick(|s: &mut UiState, i, _: &mut Tx| {
                s.orchestration.filter = Filter::ALL[i];
            })
            .build(k)
            .key("mao-state-filter"),
    )
}

fn page_empty(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let steps = [
        (
            "1",
            tr_ref(language, "Coder executes task"),
            tr_ref(
                language,
                "Your primary agent + model + effort implements and verifies changes.",
            ),
        ),
        (
            "2",
            tr_ref(language, "Auto diff & Reviewer pass"),
            tr_ref(
                language,
                "When Coder finishes, MAO gathers workspace diff and hands it to your Reviewer.",
            ),
        ),
        (
            "3",
            tr_ref(language, "Auto-rework or acceptance"),
            tr_ref(
                language,
                "Issues are routed back automatically up to your round limit, then flagged clearly.",
            ),
        ),
    ];

    let mut cards = row().gap(12.0).flex_wrap().w_full().max_w(860.0);
    for (num, title, desc) in steps {
        cards = cards.child(
            col()
                .grow(1.0)
                .min_w(230.0)
                .max_w(280.0)
                .padding(18.0)
                .gap(10.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_01)
                .border(1.0, k.c.border_weak)
                .child(
                    row()
                        .w_full()
                        .gap(10.0)
                        .center_y()
                        .child(
                            el().size(24.0, 24.0)
                                .radius(12.0)
                                .fill(k.c.bg_accent.alpha(0.16))
                                .border(1.0, k.c.bg_accent.alpha(0.45))
                                .center()
                                .child(text(num).font(k.ty.small_medium).color(k.c.text_accent)),
                        )
                        .child(
                            text(title)
                                .font(k.ty.ui_medium)
                                .color(k.c.text_base)
                                .wrap()
                                .grow(1.0)
                                .min_w(0.0),
                        ),
                )
                .child(
                    text(desc)
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full(),
                ),
        );
    }

    col()
        .key("mao-empty")
        .w_full()
        .h_full()
        .center()
        .padding(28.0)
        .gap(18.0)
        .child(
            el().size(52.0, 52.0)
                .radius(16.0)
                .fill(k.c.bg_accent.alpha(0.12))
                .border(1.0, k.c.bg_accent.alpha(0.35))
                .center()
                .child(icon(&glyphs::WORKFLOW, 26.0).color(k.c.text_accent)),
        )
        .child(
            text(tr_ref(language, "No MAO runs yet"))
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        )
        .child(cards)
        .child(
            Button::new(tr_ref(language, "Start with MAO"))
                .variant(ButtonVariant::Submit)
                .on_press(|s: &mut UiState| {
                    if !s.cfg.ui.mao.enabled {
                        toggle_enabled(s, Slot::Start);
                    }
                    s.go(Mode::Home);
                })
                .build(k)
                .key("mao-empty-cta"),
        )
}

fn page_list(s: &UiState, k: &Kit, list: &[&MaoRun], selected_id: Option<&str>) -> El {
    let language = s.cfg.general.language;
    let now = now_ms();
    let mut rows = col().key("mao-rows").w_full().padding(16.0).gap(10.0);
    for run in list {
        let is_selected = selected_id == Some(run.id.as_str());
        let id = run.id.clone();
        let coder_name = s.agent_name(&run.coder.agent);
        let rev_name = s.agent_name(&run.reviewer.agent);
        let accent = phase_color(k, &run.phase);
        let coder_live = inspect_agent_live(
            s,
            k,
            Some(run.coder_asid.as_str()),
            matches!(run.phase, MaoPhase::Coding { .. }),
            language,
        );
        let rev_live = inspect_agent_live(
            s,
            k,
            run.reviewer_asid.as_deref(),
            matches!(run.phase, MaoPhase::Reviewing { .. }),
            language,
        );
        let (adds, dels) = diff_totals(&run.files);

        let header = row()
            .w_full()
            .gap(8.0)
            .center_y()
            .child(el().size(8.0, 8.0).radius(4.0).fill(accent))
            .child(
                text(concise_excerpt(&run.goal, 96))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .truncate()
                    .grow(1.0)
                    .min_w(0.0),
            )
            .child(phase_badge(k, &run.phase, language));

        let coder_pill = row()
            .px(8.0)
            .py(4.0)
            .gap(5.0)
            .center_y()
            .radius(k.radius.sm)
            .fill(k.c.layer_02)
            .child(el().size(6.0, 6.0).radius(3.0).fill(coder_live.dot_color))
            .child(agents::mark(&run.coder.agent, 12.0, s.dark))
            .child(
                text(if run.files.is_empty() {
                    format!("{coder_name} · {}", coder_live.status_label)
                } else {
                    format!(
                        "{coder_name} · {} {}",
                        run.files.len(),
                        tr_ref(language, "changed files")
                    )
                })
                .font(k.ty.small)
                .color(k.c.text_base)
                .truncate(),
            )
            .children((!run.files.is_empty()).then(|| {
                text(format!("+{adds} -{dels}"))
                    .font(k.ty.small_medium)
                    .color(k.c.success.fg)
            }));

        let rev_pill = row()
            .px(8.0)
            .py(4.0)
            .gap(5.0)
            .center_y()
            .radius(k.radius.sm)
            .fill(k.c.layer_02)
            .child(el().size(6.0, 6.0).radius(3.0).fill(rev_live.dot_color))
            .child(agents::mark(&run.reviewer.agent, 12.0, s.dark))
            .child(
                text(format!("{rev_name} · {}", rev_live.status_label))
                    .font(k.ty.small)
                    .color(k.c.text_base)
                    .truncate(),
            );

        let meta = row()
            .w_full()
            .gap(6.0)
            .center_y()
            .flex_wrap()
            .child(coder_pill)
            .child(rev_pill)
            .child(el().grow(1.0))
            .child(
                text(format!(
                    "{}/{} · {}",
                    run.round,
                    run.max_rounds,
                    ago(run.updated_ms, now, language)
                ))
                .font(k.ty.small)
                .color(k.c.text_faint),
            );

        rows = rows.child(
            col()
                .key(format!("mao-row-{}", run.id))
                .w_full()
                .padding(14.0)
                .gap(8.0)
                .radius(k.radius.lg)
                .fill(if is_selected {
                    k.c.layer_02
                } else {
                    k.c.layer_01
                })
                .border(
                    1.0,
                    if is_selected {
                        k.c.border_focus
                    } else {
                        k.c.border_weak
                    },
                )
                .on_press(move |s: &mut UiState| {
                    s.orchestration.selected_run = Some(id.clone());
                })
                .child(header)
                .child(meta),
        );
    }
    col()
        .key("mao-list")
        .w(0.0)
        .grow(0.42)
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .child(
            col()
                .key("mao-list-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(rows)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k),
        )
}

#[allow(clippy::too_many_arguments)]
fn agent_live_card(
    s: &UiState,
    k: &Kit,
    role_title: &'static str,
    role: &MaoRole,
    asid: Option<&str>,
    live: &AgentLiveSnapshot,
    language: Language,
    key_prefix: &str,
) -> El {
    let agent_name = s.agent_name(&role.agent);
    let (model_ref, model_display) = match &role.model {
        Some(m) => {
            let d = catalog_for_agent(s, &role.agent)
                .and_then(|v| catalog_model_info(v, m))
                .map(|i| i.name.as_str());
            (Some(m.clone()), d)
        }
        None => (None, None),
    };
    let model_text = model_ref
        .as_ref()
        .map(|m| {
            let name = format_model_name(m, "Default", model_display);
            match m.variant.as_deref() {
                Some(v) => format!("{name} · {}", readable_catalog_id(v)),
                None => name,
            }
        })
        .unwrap_or_else(|| tr(language, "Agent default model"));

    let mut card = col()
        .key(format!("mao-agent-card-{key_prefix}"))
        .padding(12.0)
        .gap(6.0)
        .radius(k.radius.md)
        .fill(k.c.layer_01)
        .border(
            1.0,
            if live.waiting_attention {
                k.c.warning.icon.alpha(0.6)
            } else if live.is_busy {
                k.c.bg_accent.alpha(0.45)
            } else {
                k.c.border_weak
            },
        )
        .child(
            row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(agents::mark(&role.agent, 15.0, s.dark))
                .child(
                    text(format!("{role_title} · {agent_name}"))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(el().size(7.0, 7.0).radius(3.5).fill(live.dot_color))
                .child(
                    text(live.status_label.clone())
                        .font(k.ty.small_medium)
                        .color(k.c.text_muted),
                ),
        )
        .child(
            text(model_text)
                .font(k.ty.small)
                .color(k.c.text_faint)
                .truncate(),
        )
        .child(
            text(live.activity_line.clone())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .w_full(),
        );

    if let Some(target_id) = asid.filter(|id| !id.is_empty() && !id.starts_with("new-")) {
        let target = target_id.to_string();
        card = card.child(
            row().w_full().pt(2.0).child(
                Button::new(if live.waiting_attention {
                    tr_ref(language, "Review action")
                } else {
                    tr_ref(language, "Enter session")
                })
                .variant(if live.waiting_attention {
                    ButtonVariant::Neutral
                } else {
                    ButtonVariant::GhostMuted
                })
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.open_session(&target))
                .build(k)
                .key(format!("mao-agent-open-{key_prefix}")),
            ),
        );
    }

    card
}

fn page_detail(s: &UiState, k: &Kit, run: &MaoRun) -> El {
    let language = s.cfg.general.language;
    let coder_name = s.agent_name(&run.coder.agent);
    let reviewer_name = s.agent_name(&run.reviewer.agent);
    let coder_live = inspect_agent_live(
        s,
        k,
        Some(run.coder_asid.as_str()),
        matches!(run.phase, MaoPhase::Coding { .. }),
        language,
    );
    let rev_live = inspect_agent_live(
        s,
        k,
        run.reviewer_asid.as_deref(),
        matches!(run.phase, MaoPhase::Reviewing { .. }),
        language,
    );

    let mut actions = row().w_full().gap(8.0).flex_wrap();
    if !run.coder_asid.starts_with("new-") {
        let diff_target = run.coder_asid.clone();
        actions = actions.child(
            Button::new(tr_ref(language, "View diff"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| open_run_diff(s, &diff_target))
                .build(k)
                .key("mao-detail-diff"),
        );
    }
    if matches!(run.phase, MaoPhase::ChangesRequested { .. }) {
        let id_rework = run.id.clone();
        let id_approve = run.id.clone();
        actions = actions
            .child(
                Button::new(tr_ref(language, "Continue rework"))
                    .variant(ButtonVariant::Submit)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| continue_rework(s, &id_rework))
                    .build(k)
                    .key("mao-detail-rework"),
            )
            .child(
                Button::new(tr_ref(language, "Mark approved"))
                    .variant(ButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| mark_approved(s, &id_approve))
                    .build(k)
                    .key("mao-detail-approve"),
            );
    } else if run.is_active() {
        let id_stop = run.id.clone();
        actions = actions.child(
            Button::new(tr_ref(language, "Stop"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| cancel_run(s, &id_stop))
                .build(k)
                .key("mao-detail-stop"),
        );
    }

    let agents_pair = row()
        .w_full()
        .gap(10.0)
        .flex_wrap()
        .child(
            agent_live_card(
                s,
                k,
                tr_ref(language, "Coder"),
                &run.coder,
                Some(run.coder_asid.as_str()),
                &coder_live,
                language,
                "coder",
            )
            .w(0.0)
            .grow(1.0)
            .min_w(180.0),
        )
        .child(
            agent_live_card(
                s,
                k,
                tr_ref(language, "Reviewer"),
                &run.reviewer,
                run.reviewer_asid.as_deref(),
                &rev_live,
                language,
                "reviewer",
            )
            .w(0.0)
            .grow(1.0)
            .min_w(180.0),
        );

    let mut gates_row = row().w_full().gap(8.0).flex_wrap();
    for (i, gate) in quality_gates_for_run(run, language).into_iter().enumerate() {
        let (fill, border, ink) = gate_status_colors(k, gate.status);
        gates_row = gates_row.child(
            col()
                .key(format!("mao-gate-{i}"))
                .grow(1.0)
                .min_w(130.0)
                .px(10.0)
                .py(8.0)
                .gap(2.0)
                .radius(k.radius.md)
                .fill(fill)
                .border(1.0, border)
                .child(text(gate.title).font(k.ty.small).color(ink.alpha(0.85)))
                .child(text(gate.detail).font(k.ty.small_medium).color(ink)),
        );
    }

    let diff_target = (!run.coder_asid.starts_with("new-")).then(|| run.coder_asid.clone());

    let repo_count = run.base_commits.len();
    let mut files_box = col()
        .w_full()
        .padding(12.0)
        .gap(6.0)
        .radius(k.radius.md)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    text(tr_ref(language, "Changed files"))
                        .font(k.ty.small_medium)
                        .color(k.c.text_faint)
                        .grow(1.0),
                )
                .children((repo_count > 1).then(|| {
                    Badge::new(format!("{repo_count} {}", tr_ref(language, "repos")))
                        .variant(BadgeVariant::Subtle)
                        .build(k)
                }))
                .children((!run.files.is_empty()).then(|| {
                    let (adds, dels) = diff_totals(&run.files);
                    row()
                        .gap(6.0)
                        .child(
                            text(format!("+{adds}"))
                                .font(k.ty.small_medium)
                                .color(k.c.success.fg),
                        )
                        .child(
                            text(format!("-{dels}"))
                                .font(k.ty.small_medium)
                                .color(k.c.danger.fg),
                        )
                })),
        );
    if run.files.is_empty() {
        files_box = files_box.child(
            text(tr_ref(language, "No changed files captured yet."))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    } else {
        for (idx, file) in run.files.iter().take(12).enumerate() {
            let mut frow = row()
                .key(format!("mao-file-{idx}"))
                .w_full()
                .gap(8.0)
                .py(2.0)
                .center_y()
                .child(
                    text(file.path.clone())
                        .font(k.ty.mono)
                        .color(k.c.text_base)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .child(
                    text(format!("+{}", file.additions))
                        .font(k.ty.small_medium)
                        .color(k.c.success.fg),
                )
                .child(
                    text(format!("-{}", file.deletions))
                        .font(k.ty.small_medium)
                        .color(k.c.danger.fg),
                );
            if let Some(ref target) = diff_target {
                let target = target.clone();
                frow = frow.on_press(move |s: &mut UiState| open_run_diff(s, &target));
            }
            files_box = files_box.child(frow);
        }
    }

    let findings_box = (!run.findings.is_empty()).then(|| {
        let mut box_col = col()
            .key("mao-findings-box")
            .w_full()
            .padding(12.0)
            .gap(6.0)
            .radius(k.radius.md)
            .fill(k.c.layer_01)
            .border(1.0, k.c.border_weak)
            .child(
                row()
                    .w_full()
                    .center_y()
                    .gap(6.0)
                    .child(
                        text(tr_ref(language, "Review findings"))
                            .font(k.ty.small_medium)
                            .color(k.c.text_faint)
                            .grow(1.0),
                    )
                    .child(
                        Badge::new(run.findings.len().to_string())
                            .variant(BadgeVariant::Subtle)
                            .build(k),
                    ),
            );
        for (idx, finding) in run.findings.iter().take(12).enumerate() {
            let (sev_fill, sev_border, sev_ink) = match finding.severity.as_str() {
                "high" => (
                    k.c.danger.icon.alpha(0.14),
                    k.c.danger.icon.alpha(0.45),
                    k.c.danger.fg,
                ),
                "low" => (
                    k.c.info.icon.alpha(0.12),
                    k.c.info.icon.alpha(0.35),
                    k.c.text_muted,
                ),
                _ => (
                    k.c.warning.icon.alpha(0.14),
                    k.c.warning.icon.alpha(0.45),
                    k.c.warning.fg,
                ),
            };
            let sev_badge = el()
                .px(6.0)
                .py(1.0)
                .radius(k.radius.sm)
                .fill(sev_fill)
                .border(1.0, sev_border)
                .child(
                    text(finding.severity.to_ascii_uppercase())
                        .font(k.ty.small_medium)
                        .color(sev_ink),
                );
            let mut item_col = col()
                .key(format!("mao-finding-{idx}"))
                .w_full()
                .px(8.0)
                .py(6.0)
                .gap(3.0)
                .radius(k.radius.sm)
                .fill(k.c.layer_02)
                .child(
                    row().w_full().gap(6.0).center_y().child(sev_badge).child(
                        text(finding.location.clone())
                            .font(k.ty.mono)
                            .color(k.c.text_base)
                            .truncate()
                            .grow(1.0)
                            .min_w(0.0),
                    ),
                )
                .child(
                    text(finding.message.clone())
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full(),
                );
            if let Some(ref target) = diff_target {
                let target = target.clone();
                item_col = item_col.on_press(move |s: &mut UiState| open_run_diff(s, &target));
            }
            box_col = box_col.child(item_col);
        }
        box_col
    });

    let coder_report = col()
        .grow(1.0)
        .min_w(210.0)
        .padding(12.0)
        .gap(4.0)
        .radius(k.radius.md)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .child(
            text(tr_ref(language, "Coder summary"))
                .font(k.ty.small_medium)
                .color(k.c.text_faint),
        )
        .child(
            text(if run.coder_summary.is_empty() {
                tr(
                    language,
                    "Waiting for Coder to finish before collecting deliverables.",
                )
            } else {
                run.coder_summary.clone()
            })
            .font(k.ty.small)
            .color(k.c.text_base)
            .wrap()
            .w_full(),
        );

    let review_report = col()
        .grow(1.0)
        .min_w(210.0)
        .padding(12.0)
        .gap(4.0)
        .radius(k.radius.md)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .child(
            text(tr_ref(language, "Review report"))
                .font(k.ty.small_medium)
                .color(k.c.text_faint),
        )
        .child(
            text(if run.review_summary.is_empty() {
                tr(language, "Waiting for Reviewer report.")
            } else {
                run.review_summary.clone()
            })
            .font(k.ty.small)
            .color(k.c.text_base)
            .wrap()
            .w_full(),
        );

    let mut steps_col = col().w_full().gap(6.0);
    for (idx, step) in run.steps.iter().enumerate() {
        steps_col = steps_col.child(step_row(k, idx, step));
    }

    let content = col()
        .key(format!("mao-detail-body-{}", run.id))
        .w_full()
        .padding(20.0)
        .gap(14.0)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(phase_badge(k, &run.phase, language))
                .child(
                    text(format!(
                        "{} {}/{}",
                        tr_ref(language, "Review round"),
                        run.round,
                        run.max_rounds
                    ))
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted),
                )
                .child(el().grow(1.0))
                .child(actions),
        )
        .child(
            text(run.goal.clone())
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .wrap()
                .w_full(),
        )
        .child(
            text(phase_summary_line(
                run,
                &coder_name,
                &reviewer_name,
                language,
            ))
            .font(k.ty.small)
            .color(k.c.text_muted)
            .wrap()
            .w_full(),
        )
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(
                    text(tr_ref(language, "Agent live status"))
                        .font(k.ty.small_medium)
                        .color(k.c.text_faint),
                )
                .child(agents_pair),
        )
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(
                    text(tr_ref(language, "Quality gates"))
                        .font(k.ty.small_medium)
                        .color(k.c.text_faint),
                )
                .child(gates_row),
        )
        .child(
            col()
                .w_full()
                .gap(8.0)
                .child(
                    text(tr_ref(language, "Deliverables & review report"))
                        .font(k.ty.small_medium)
                        .color(k.c.text_faint),
                )
                .child(files_box)
                .children(findings_box)
                .child(
                    row()
                        .w_full()
                        .gap(10.0)
                        .flex_wrap()
                        .child(coder_report)
                        .child(review_report),
                ),
        )
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(
                    text(tr_ref(language, "Pipeline timeline"))
                        .font(k.ty.small_medium)
                        .color(k.c.text_faint),
                )
                .child(steps_col),
        );

    col()
        .key(format!("mao-detail-{}", run.id))
        .w(0.0)
        .grow(0.58)
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .child(
            col()
                .key("mao-detail-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(content)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k),
        )
}

fn add_pool_entry(s: &mut UiState, entry: MaoRoleConfig) {
    let mut next = s.cfg.ui.mao.clone();
    let already_exists = next.agent_pool.iter().any(|item| {
        item.agent == entry.agent && item.model == entry.model && item.machine == entry.machine
    });
    if !already_exists && next.agent_pool.len() < 12 {
        next.agent_pool.push(entry);
        save_mao_prefs(s, next);
    }
    s.menu = None;
}

fn import_connected_agents_to_pool(s: &mut UiState) {
    let candidates: Vec<String> = usable_agents(s).into_iter().map(|a| a.id).collect();
    if candidates.is_empty() {
        return;
    }
    let mut next = s.cfg.ui.mao.clone();
    for id in candidates {
        if next.agent_pool.len() >= 12 {
            break;
        }
        if !next.agent_pool.iter().any(|item| item.agent == id) {
            next.agent_pool.push(MaoRoleConfig {
                agent: id,
                model: None,
                machine: None,
            });
        }
    }
    save_mao_prefs(s, next);
}

fn move_pool_entry(s: &mut UiState, idx: usize, delta: isize) {
    let mut next = s.cfg.ui.mao.clone();
    let target = idx as isize + delta;
    if idx < next.agent_pool.len() && target >= 0 && (target as usize) < next.agent_pool.len() {
        next.agent_pool.swap(idx, target as usize);
        save_mao_prefs(s, next);
    }
}

fn remove_pool_entry(s: &mut UiState, idx: usize) {
    let mut next = s.cfg.ui.mao.clone();
    if idx < next.agent_pool.len() {
        next.agent_pool.remove(idx);
        save_mao_prefs(s, next);
    }
}

fn pool_add_menu(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let open = s.menu == Some(OpenMenu::MaoPoolAdd);
    let active_agent = s.orchestration.pool_add_agent.clone();
    let dark = s.dark;

    let trigger = Button::new(tr_ref(language, "+ Add Agent / Model"))
        .variant(ButtonVariant::Neutral)
        .control_size(ControlSize::Small)
        .expanded(open)
        .on_press(move |s: &mut UiState| {
            if open {
                s.menu = None;
            } else {
                let first = usable_agents(s).first().map(|a| a.id.clone());
                if let Some(ref aid) = first {
                    ensure_reviewer_catalog(s, aid);
                }
                s.orchestration.pool_add_agent = first;
                s.menu = Some(OpenMenu::MaoPoolAdd);
            }
        })
        .build(k)
        .key("settings-mao-pool-add-btn");

    let mut menu = Menu::new("settings-mao-pool-add")
        .placement(Placement::BottomStart)
        .trigger(trigger)
        .open(open)
        .min_width(280.0)
        .on_close(|s: &mut UiState| s.menu = None);

    let agents_list = usable_agents(s);
    if agents_list.is_empty() {
        menu = menu
            .item(MenuItem::new(tr_ref(language, "No connected agents available")).disabled(true));
    } else {
        menu = menu.group_label(tr_ref(language, "Choose Agent & Model"));
        for agent in agents_list {
            let aid = agent.id.clone();
            let expanded = active_agent.as_deref() == Some(aid.as_str());
            let models = model_options(s, &aid);
            let distinct_providers = models
                .iter()
                .map(|(p, _, _, _)| p.as_str())
                .collect::<HashSet<_>>()
                .len();

            let mut sub_entries = Vec::new();
            let aid_default = aid.clone();
            sub_entries.push(MenuEntry::item(
                MenuItem::new(tr_ref(language, "Use agent default")).on_select(
                    move |s: &mut UiState| {
                        add_pool_entry(
                            s,
                            MaoRoleConfig {
                                agent: aid_default.clone(),
                                model: None,
                                machine: None,
                            },
                        );
                    },
                ),
            ));
            let mut last_provider: Option<String> = None;
            for (provider, model_id, display, _) in models.into_iter().take(24) {
                if distinct_providers > 1 && last_provider.as_deref() != Some(provider.as_str()) {
                    sub_entries.push(MenuEntry::Label(provider.clone()));
                    last_provider = Some(provider.clone());
                }
                let aid_m = aid.clone();
                sub_entries.push(MenuEntry::item(MenuItem::new(display).on_select(
                    move |s: &mut UiState| {
                        add_pool_entry(
                            s,
                            MaoRoleConfig {
                                agent: aid_m.clone(),
                                model: Some(ModelRef {
                                    provider_id: provider.clone(),
                                    model_id: model_id.clone(),
                                    variant: None,
                                }),
                                machine: None,
                            },
                        );
                    },
                )));
            }

            let aid_select = aid.clone();
            menu = menu.item(
                MenuItem::new(s.agent_name(&aid))
                    .lead(agents::mark(&aid, 14.0, dark))
                    .submenu(sub_entries, expanded)
                    .on_select(move |s: &mut UiState| {
                        ensure_reviewer_catalog(s, &aid_select);
                        s.orchestration.pool_add_agent = Some(aid_select.clone());
                        s.menu = Some(OpenMenu::MaoPoolAdd);
                    }),
            );
        }
    }

    menu.build(k, layer)
}

/// Settings → Agents configuration card for Smart Orchestration defaults,
/// user-configured Agent/Model candidate pool, and automatic quota failover.
pub fn settings_card(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let enabled = s.cfg.ui.mao.enabled;
    let auto_failover = s.cfg.ui.mao.auto_failover;
    let coder_id = s.start_agent.clone();
    let reviewer = effective_reviewer(s, &coder_id);
    let max_rounds = s.cfg.ui.mao.max_rounds.clamp(1, 5);
    let reviewer_menu = reviewer_config_menu(
        s,
        k,
        layer,
        MaoMenuAnchor::Settings,
        &coder_id,
        &reviewer,
        max_rounds,
        Placement::BottomStart,
    );
    let add_pool_btn = pool_add_menu(s, k, layer);

    let mut pool_list = col().w_full().gap(6.0);
    if s.cfg.ui.mao.agent_pool.is_empty() {
        pool_list = pool_list.child(
            row()
                .w_full()
                .px(10.0)
                .py(8.0)
                .gap(8.0)
                .center_y()
                .flex_wrap()
                .radius(k.radius.md)
                .fill(k.c.layer_01)
                .border(1.0, k.c.border_weak)
                .child(
                    text(tr_ref(
                        language,
                        "No agents in pool yet. Add preferred Agents & Models so the orchestrator can allocate tasks dynamically and auto-switch on quota limits.",
                    ))
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .wrap()
                    .grow(1.0)
                    .min_w(180.0),
                )
                .child(
                    Button::new(tr_ref(language, "Import connected agents"))
                        .variant(ButtonVariant::Neutral)
                        .control_size(ControlSize::Small)
                        .on_press(import_connected_agents_to_pool)
                        .build(k)
                        .key("settings-mao-pool-import"),
                ),
        );
    } else {
        let total = s.cfg.ui.mao.agent_pool.len();
        for (idx, item) in s.cfg.ui.mao.agent_pool.iter().enumerate() {
            let agent_name = s.agent_name(&item.agent);
            let model_label = item
                .model
                .as_ref()
                .map(|m| format_model_name(m, "Default", None))
                .unwrap_or_else(|| tr(language, "Agent default model"));
            let mut sub_label = model_label;
            if let Some(v) = item.model.as_ref().and_then(|m| m.variant.as_deref()) {
                sub_label.push_str(" · ");
                sub_label.push_str(&readable_catalog_id(v));
            }
            if let Some(machine) = item.machine.as_deref() {
                sub_label.push_str(" @ ");
                sub_label.push_str(machine);
            }
            pool_list = pool_list.child(
                row()
                    .key(format!("settings-mao-pool-row-{idx}"))
                    .w_full()
                    .px(10.0)
                    .py(6.0)
                    .gap(8.0)
                    .center_y()
                    .radius(k.radius.md)
                    .fill(k.c.layer_01)
                    .border(1.0, k.c.border_weak)
                    .child(
                        Badge::new(format!("#{}", idx + 1))
                            .variant(BadgeVariant::Subtle)
                            .build(k),
                    )
                    .child(agents::mark(&item.agent, 14.0, s.dark))
                    .child(
                        text(agent_name)
                            .font(k.ty.small_medium)
                            .color(k.c.text_base)
                            .single_line(),
                    )
                    .child(
                        text(sub_label)
                            .font(k.ty.small)
                            .color(k.c.text_muted)
                            .truncate()
                            .grow(1.0)
                            .min_w(0.0),
                    )
                    .child(
                        Button::new("↑")
                            .variant(ButtonVariant::GhostMuted)
                            .control_size(ControlSize::Small)
                            .disabled(idx == 0)
                            .on_press(move |s: &mut UiState| move_pool_entry(s, idx, -1))
                            .build(k)
                            .key(format!("settings-mao-pool-up-{idx}")),
                    )
                    .child(
                        Button::new("↓")
                            .variant(ButtonVariant::GhostMuted)
                            .control_size(ControlSize::Small)
                            .disabled(idx + 1 >= total)
                            .on_press(move |s: &mut UiState| move_pool_entry(s, idx, 1))
                            .build(k)
                            .key(format!("settings-mao-pool-down-{idx}")),
                    )
                    .child(
                        Button::new("×")
                            .variant(ButtonVariant::GhostMuted)
                            .control_size(ControlSize::Small)
                            .on_press(move |s: &mut UiState| remove_pool_entry(s, idx))
                            .build(k)
                            .key(format!("settings-mao-pool-rm-{idx}")),
                    ),
            );
        }
    }

    col()
        .key("settings-mao-card")
        .w_full()
        .gap(12.0)
        .px(16.0)
        .py(16.0)
        .radius(k.radius.lg)
        .fill(k.c.layer_01.alpha(0.3))
        .border(1.0, k.c.border_weak)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(icon(&glyphs::WORKFLOW, 16.0).color(k.c.text_accent))
                .child(
                    text(tr_ref(language, "MAO · Orchestration"))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(
                    Button::new(if enabled {
                        tr_ref(language, "MAO enabled")
                    } else {
                        tr_ref(language, "Enable MAO")
                    })
                    .variant(if enabled {
                        ButtonVariant::Submit
                    } else {
                        ButtonVariant::Neutral
                    })
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| toggle_enabled(s, Slot::Start))
                    .build(k)
                    .key("settings-mao-toggle"),
                ),
        )
        .child(
            text(tr_ref(
                language,
                "Orchestrator dynamically assigns tasks across your pool and automatically switches to the next configured Agent/Model when quota runs out.",
            ))
            .font(k.ty.small)
            .color(k.c.text_muted)
            .wrap()
            .w_full(),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .flex_wrap()
                .child(reviewer_menu)
                .child(
                    Button::new(if auto_failover {
                        tr_ref(language, "Auto-failover: On")
                    } else {
                        tr_ref(language, "Auto-failover: Off")
                    })
                    .variant(if auto_failover {
                        ButtonVariant::Neutral
                    } else {
                        ButtonVariant::GhostMuted
                    })
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| {
                        let mut next = s.cfg.ui.mao.clone();
                        next.auto_failover = !next.auto_failover;
                        save_mao_prefs(s, next);
                    })
                    .build(k)
                    .key("settings-mao-failover-toggle"),
                )
                .child(
                    Button::new(tr_ref(language, "Open MAO dashboard"))
                        .variant(ButtonVariant::GhostMuted)
                        .control_size(ControlSize::Small)
                        .on_press(|s: &mut UiState| s.go(Mode::Mao))
                        .build(k)
                        .key("settings-mao-open"),
                ),
        )
        .child(
            col()
                .w_full()
                .gap(8.0)
                .pt(4.0)
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(
                            text(tr_ref(language, "Agent & model pool"))
                                .font(k.ty.small_medium)
                                .color(k.c.text_base)
                                .grow(1.0)
                                .min_w(0.0),
                        )
                        .child(add_pool_btn),
                )
                .child(pool_list),
        )
}

/// Multi-surface plugin entry so MAO can run inside `Page`, split `Tile`, or right-hand `Dock` Window.
pub fn window_view(
    s: &UiState,
    k: &Kit,
    surface: crate::plugin::PluginSlot,
    layer: &mut Vec<El>,
) -> El {
    match surface {
        crate::plugin::PluginSlot::Page => page_view(s, k, layer),
        _ => window_compact_view(s, k, layer),
    }
}

fn window_compact_view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let toggle_slot = if s.selected.is_some() {
        Slot::Session
    } else {
        Slot::Start
    };
    let enabled = is_slot_enabled(s, toggle_slot);
    let coder_id = active_coder_agent(s, toggle_slot);
    let toggle_asid = slot_asid(s, toggle_slot);
    let reviewer = effective_reviewer_for(s, &coder_id, toggle_asid);
    let max_rounds = effective_max_rounds(s, toggle_asid);
    let run = s
        .selected
        .as_deref()
        .and_then(|asid| s.orchestration.run_for_session(asid))
        .or_else(|| {
            s.orchestration
                .selected_run
                .as_deref()
                .and_then(|id| s.orchestration.runs.iter().find(|r| r.id == id))
        })
        .or_else(|| s.orchestration.runs.last());

    let mut content = col()
        .key("mao-window-body")
        .w_full()
        .padding(14.0)
        .gap(12.0);
    content = content
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(icon(&glyphs::WORKFLOW, 15.0).color(k.c.text_accent))
                .child(
                    text(tr_ref(language, "MAO · Orchestration"))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(
                    Button::new(if enabled {
                        tr_ref(language, "MAO enabled")
                    } else {
                        tr_ref(language, "Enable MAO")
                    })
                    .variant(if enabled {
                        ButtonVariant::Submit
                    } else {
                        ButtonVariant::Neutral
                    })
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| toggle_enabled(s, toggle_slot))
                    .build(k)
                    .key("mao-window-toggle"),
                ),
        )
        .child(reviewer_config_menu(
            s,
            k,
            layer,
            MaoMenuAnchor::Dock(toggle_slot),
            &coder_id,
            &reviewer,
            max_rounds,
            Placement::BottomStart,
        ));

    if s.orchestration.runs.len() > 1 {
        let mut switcher = row().w_full().gap(6.0).flex_wrap();
        for item in s.orchestration.runs.iter().rev().take(6) {
            let active = run.is_some_and(|cur| cur.id == item.id);
            let id = item.id.clone();
            switcher = switcher.child(
                Button::new(concise_excerpt(&item.goal, 22))
                    .variant(if active {
                        ButtonVariant::Neutral
                    } else {
                        ButtonVariant::GhostMuted
                    })
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        s.orchestration.selected_run = Some(id.clone());
                    })
                    .build(k)
                    .key(format!("mao-win-pick-{}", item.id)),
            );
        }
        content = content.child(switcher);
    }

    if let Some(run) = run {
        let coder_name = s.agent_name(&run.coder.agent);
        let reviewer_name = s.agent_name(&run.reviewer.agent);
        let coder_live = inspect_agent_live(
            s,
            k,
            Some(run.coder_asid.as_str()),
            matches!(run.phase, MaoPhase::Coding { .. }),
            language,
        );
        let rev_live = inspect_agent_live(
            s,
            k,
            run.reviewer_asid.as_deref(),
            matches!(run.phase, MaoPhase::Reviewing { .. }),
            language,
        );

        let mut gates = row().w_full().gap(6.0).flex_wrap();
        for (i, gate) in quality_gates_for_run(run, language).into_iter().enumerate() {
            let (fill, border, ink) = gate_status_colors(k, gate.status);
            gates = gates.child(
                col()
                    .key(format!("mao-win-gate-{i}"))
                    .grow(1.0)
                    .min_w(120.0)
                    .px(8.0)
                    .py(6.0)
                    .gap(2.0)
                    .radius(k.radius.sm)
                    .fill(fill)
                    .border(1.0, border)
                    .child(text(gate.title).font(k.ty.small).color(ink.alpha(0.85)))
                    .child(text(gate.detail).font(k.ty.small_medium).color(ink)),
            );
        }

        let mut steps_col = col().w_full().gap(4.0);
        for (idx, step) in run.steps.iter().rev().take(6).rev().enumerate() {
            steps_col = steps_col.child(step_row(k, idx, step));
        }

        content = content
            .child(
                row()
                    .w_full()
                    .gap(8.0)
                    .center_y()
                    .child(phase_badge(k, &run.phase, language))
                    .child(
                        text(format!(
                            "{} {}/{}",
                            tr_ref(language, "Review round"),
                            run.round,
                            run.max_rounds
                        ))
                        .font(k.ty.small_medium)
                        .color(k.c.text_muted),
                    )
                    .child(el().grow(1.0))
                    .child(
                        Button::new(tr_ref(language, "Overview"))
                            .variant(ButtonVariant::GhostMuted)
                            .control_size(ControlSize::Small)
                            .on_press(|s: &mut UiState| s.go(Mode::Mao))
                            .build(k)
                            .key("mao-win-full"),
                    ),
            )
            .child(
                text(run.goal.clone())
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .wrap()
                    .w_full(),
            )
            .child(
                text(phase_summary_line(
                    run,
                    &coder_name,
                    &reviewer_name,
                    language,
                ))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .w_full(),
            )
            .child(
                col()
                    .w_full()
                    .gap(8.0)
                    .child(
                        agent_live_card(
                            s,
                            k,
                            tr_ref(language, "Coder"),
                            &run.coder,
                            Some(run.coder_asid.as_str()),
                            &coder_live,
                            language,
                            "win-coder",
                        )
                        .w_full(),
                    )
                    .child(
                        agent_live_card(
                            s,
                            k,
                            tr_ref(language, "Reviewer"),
                            &run.reviewer,
                            run.reviewer_asid.as_deref(),
                            &rev_live,
                            language,
                            "win-reviewer",
                        )
                        .w_full(),
                    ),
            )
            .child(gates)
            .children((!run.files.is_empty()).then(|| {
                let mut files_col = col()
                    .w_full()
                    .padding(10.0)
                    .gap(4.0)
                    .radius(k.radius.md)
                    .fill(k.c.layer_01)
                    .border(1.0, k.c.border_weak)
                    .child(
                        text(tr_ref(language, "Changed files"))
                            .font(k.ty.small_medium)
                            .color(k.c.text_faint),
                    );
                for file in run.files.iter().take(8) {
                    files_col = files_col.child(
                        row()
                            .w_full()
                            .gap(6.0)
                            .child(
                                text(file.path.clone())
                                    .font(k.ty.mono)
                                    .color(k.c.text_base)
                                    .truncate()
                                    .grow(1.0)
                                    .min_w(0.0),
                            )
                            .child(
                                text(format!("+{}", file.additions))
                                    .font(k.ty.small_medium)
                                    .color(k.c.success.fg),
                            )
                            .child(
                                text(format!("-{}", file.deletions))
                                    .font(k.ty.small_medium)
                                    .color(k.c.danger.fg),
                            ),
                    );
                }
                files_col
            }))
            .children((!run.review_summary.is_empty()).then(|| {
                col()
                    .w_full()
                    .padding(10.0)
                    .gap(4.0)
                    .radius(k.radius.md)
                    .fill(k.c.layer_01)
                    .border(1.0, k.c.border_weak)
                    .child(
                        text(tr_ref(language, "Review report"))
                            .font(k.ty.small_medium)
                            .color(k.c.text_faint),
                    )
                    .child(
                        text(run.review_summary.clone())
                            .font(k.ty.small)
                            .color(k.c.text_base)
                            .wrap()
                            .w_full(),
                    )
            }))
            .child(steps_col);
    } else {
        content = content.child(page_empty(s, k));
    }

    col().key("mao-window").w_full().h_full().min_h(0.0).child(
        col()
            .key("mao-window-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(content)
            .scroll(rune::ScrollSpec {
                stick_bottom: false,
            })
            .scroll_bar(k),
    )
}

fn open_run_diff(s: &mut UiState, target: &str) {
    s.open_session(target);
    s.show_diff = true;
    s.send(Command::RefreshDiff(AgentSessionId(target.to_string())));
}

pub fn is_conversational_query(prompt: &str) -> bool {
    let trimmed = prompt.trim();
    if trimmed.is_empty() || trimmed.len() > 260 {
        return false;
    }
    let lower = trimmed.to_lowercase();
    // Explicit imperative requests to edit/implement/fix should still use MAO.
    const IMPERATIVE_EDIT_HINTS: &[&str] = &[
        "帮我改",
        "帮我修",
        "去改",
        "去修",
        "修复",
        "重构",
        "实现",
        "补齐",
        "补全",
        "加上",
        "新增",
        "删除",
        "替换",
        "优化一下",
        "改造",
        "please fix",
        "can you fix",
        "please implement",
        "please add",
        "please update",
        "please refactor",
    ];
    if IMPERATIVE_EDIT_HINTS.iter().any(|k| lower.contains(k)) {
        return false;
    }
    if trimmed.ends_with('?') || trimmed.ends_with('？') {
        return true;
    }
    const QUERY_PREFIXES: &[&str] = &[
        "你改了什么",
        "改了什么",
        "还差哪些",
        "还有哪些",
        "为什么",
        "怎么",
        "在哪里",
        "在哪",
        "哪里",
        "什么问题",
        "看看",
        "查一下",
        "是不是",
        "会不会",
        "有没有",
        "权限管理做哪里",
        "what ",
        "why ",
        "how ",
        "where ",
        "explain ",
        "which ",
    ];
    QUERY_PREFIXES
        .iter()
        .any(|p| lower.starts_with(p) || lower.contains(p))
}

pub fn gateway_dispatch_available(s: &UiState) -> bool {
    !s.demo
        && !s.hub_label.is_empty()
        && !s.hub_label.eq_ignore_ascii_case("standalone")
        && !s.hub_label.eq_ignore_ascii_case("demo")
}

fn format_gateway_model_id(model: &ModelRef) -> String {
    if model.provider_id.is_empty() {
        model.model_id.clone()
    } else {
        format!("{}/{}", model.provider_id, model.model_id)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn dispatch_gateway_start(
    s: &mut UiState,
    pending_or_asid: &str,
    token: u64,
    prompt: &str,
    cwd: &str,
    coder_agent: &str,
    coder_model: Option<&ModelRef>,
    coder_mode: Option<&str>,
    existing_asid: Option<&str>,
    attachments: &[String],
) -> bool {
    if !gateway_dispatch_available(s) {
        return false;
    }
    let computer = s.current_computer().to_string();
    if computer.is_empty() || computer == "all" {
        return false;
    }
    let trimmed_cwd = cwd.trim();
    if trimmed_cwd.is_empty() {
        return false;
    }
    let reviewer = effective_reviewer_for(s, coder_agent, existing_asid);
    ensure_reviewer_catalog(s, &reviewer.agent);
    let max_rounds = effective_max_rounds(s, existing_asid);
    let auto_failover = effective_auto_failover(s, existing_asid);
    let now = now_ms();
    let client_ref = format!("desktop-{token}-{now}");
    let (resolved_rev_model, _) = resolved_reviewer_model(s, &reviewer);
    let dispatch_rev_model = reviewer.model.as_ref().or(resolved_rev_model.as_ref());
    let mut rev_obj = serde_json::json!({
        "agent": reviewer.agent,
    });
    if let Some(m) = dispatch_rev_model {
        rev_obj["model"] = serde_json::json!(format_gateway_model_id(m));
        if let Some(v) = m.variant.as_deref() {
            rev_obj["variant"] = serde_json::json!(v);
        }
    }
    if let Some(machine) = reviewer.machine.as_deref() {
        rev_obj["machine"] = serde_json::json!(machine);
    }

    let pool_json: Vec<serde_json::Value> = s
        .cfg
        .ui
        .mao
        .agent_pool
        .iter()
        .filter(|entry| !entry.agent.trim().is_empty())
        .take(12)
        .map(|entry| {
            let mut item = serde_json::json!({ "agent": entry.agent });
            if let Some(m) = entry.model.as_ref() {
                item["model"] = serde_json::json!(format_gateway_model_id(m));
                if let Some(v) = m.variant.as_deref() {
                    item["variant"] = serde_json::json!(v);
                }
            }
            if let Some(machine) = entry.machine.as_deref() {
                item["machine"] = serde_json::json!(machine);
            }
            item
        })
        .collect();

    let mut body = serde_json::json!({
        "client_ref": client_ref,
        "goal": prompt,
        "cwd": trimmed_cwd,
        "coordinator": coder_agent,
        "model": coder_model.map(format_gateway_model_id),
        "variant": coder_model.and_then(|m| m.variant.clone()),
        "attachments": attachments,
        "auto_approve": true,
        "reviewer": rev_obj,
        "max_review_rounds": max_rounds,
        "auto_failover": auto_failover,
    });
    if !pool_json.is_empty() {
        body["agent_pool"] = serde_json::Value::Array(pool_json);
    }
    if let Some(mode) = coder_mode {
        body["mode"] = serde_json::json!(mode);
    }
    if let Some(asid) = existing_asid {
        preserve_session_title(s, asid);
        body["asid"] = serde_json::json!(asid);
    }

    let ticket = s.next_token;
    s.next_token += 1;
    s.orchestration
        .gateway_start_tokens
        .insert(ticket, (pending_or_asid.to_string(), token));
    s.orchestration
        .enabled_sessions
        .insert(pending_or_asid.to_string());
    s.send(Command::Orchestration {
        computer,
        ticket,
        request: crate::hub::OrchestrationRequest::Start(body),
    });
    true
}

pub fn request_gateway_read(s: &mut UiState, run_id: &str) {
    let computer = s.current_computer().to_string();
    if computer.is_empty() || computer == "all" {
        return;
    }
    let ticket = s.next_token;
    s.next_token += 1;
    s.send(Command::Orchestration {
        computer,
        ticket,
        request: crate::hub::OrchestrationRequest::Read(run_id.to_string()),
    });
}

/// Automatically refresh active Gateway-managed orchestrations (including
/// cross-machine Reviewers whose sessions do not emit local Idle events).
pub fn poll_active_gateway_runs(s: &mut UiState, force: bool) {
    if !gateway_dispatch_available(s) {
        return;
    }
    let active_ids: Vec<String> = s
        .orchestration
        .runs
        .iter()
        .rev()
        .filter(|r| r.id.starts_with("o_") && r.is_active())
        .take(4)
        .map(|r| r.id.clone())
        .collect();
    if active_ids.is_empty() {
        return;
    }
    let now = now_ms();
    let interval_ms = if force { 1_000 } else { 3_000 };
    if now.saturating_sub(s.orchestration.last_gateway_poll_ms) < interval_ms {
        return;
    }
    s.orchestration.last_gateway_poll_ms = now;
    for id in active_ids {
        request_gateway_read(s, &id);
    }
}

pub fn ensure_gateway_synced(s: &mut UiState) {
    let computer = s.current_computer().to_string();
    if computer.is_empty() || computer == "all" {
        return;
    }
    if s.orchestration.synced_computers.insert(computer) {
        request_gateway_sync(s);
    }
}

pub fn request_gateway_sync(s: &mut UiState) {
    let computer = s.current_computer().to_string();
    if computer.is_empty() || computer == "all" {
        return;
    }
    let ticket = s.next_token;
    s.next_token += 1;
    s.send(Command::Orchestration {
        computer,
        ticket,
        request: crate::hub::OrchestrationRequest::List,
    });
}

fn adopt_gateway_coder_session(
    s: &mut UiState,
    pending_asid: &str,
    token: u64,
    real_asid: &str,
    agent_id: &str,
    dir: Option<String>,
    goal: Option<&str>,
) {
    let switched_to_new_session = pending_asid.starts_with("new-") || real_asid != pending_asid;
    if switched_to_new_session {
        let title = goal
            .map(|g| concise_excerpt(g, 40))
            .unwrap_or_else(|| agent_id.to_string());
        if s.session_by_id(real_asid).is_none() {
            if let Ok(stub) = serde_json::from_value(serde_json::json!({
                "asid": real_asid,
                "agent_id": agent_id,
                "title": title,
                "directory": dir,
                "status": "busy",
                "updated_ms": now_ms(),
            })) {
                s.sessions.insert(0, stub);
            }
        }
        if pending_asid.starts_with("new-")
            && s.pending_start.as_ref().is_some_and(|(_, t)| *t == token)
        {
            s.adopt_created_session(real_asid);
        } else if real_asid != pending_asid {
            s.open_session(real_asid);
        }
        s.echo_submitted(token, true);
        s.echo_rows(real_asid);
        if pending_asid.starts_with("new-") {
            s.orchestration.enabled_sessions.remove(pending_asid);
        }
        if s.cfg.ui.mao.session_overrides.get(real_asid) != Some(&false) {
            s.orchestration
                .enabled_sessions
                .insert(real_asid.to_string());
            set_session_mao_override(s, real_asid, true);
        }
        s.auto_approve.insert(real_asid.to_string(), true);
        s.send(Command::FocusSession(AgentSessionId(real_asid.to_string())));
        if let Some(info) = s.session_by_id(real_asid).cloned() {
            s.session_opened(&info);
        }
    } else {
        s.echo_submitted(token, true);
        s.echo_rows(real_asid);
        if s.cfg.ui.mao.session_overrides.get(real_asid) != Some(&false) {
            s.orchestration
                .enabled_sessions
                .insert(real_asid.to_string());
        }
        s.auto_approve.insert(real_asid.to_string(), true);
    }
    s.status = "Running…".into();
    s.sync_watch();
}

pub fn on_gateway_orchestration(
    s: &mut UiState,
    computer: &str,
    ticket: u64,
    result: Result<serde_json::Value, crate::hub::OrchestrationError>,
) {
    let pending_start = s.orchestration.gateway_start_tokens.remove(&ticket);
    let payload = match result {
        Ok(payload) => payload,
        Err(err) => {
            if let Some((pending_asid, token)) = pending_start {
                s.echo_submitted(token, false);
                if s.pending_start.as_ref().is_some_and(|(_, t)| *t == token) {
                    s.pending_start = None;
                }
                if pending_asid.starts_with("new-") {
                    s.sessions.retain(|item| item.asid.0 != pending_asid);
                }
                s.notify(
                    ToastLevel::Error,
                    tr(s.cfg.general.language, "Failed to start orchestration"),
                    Some(err.message),
                );
            }
            return;
        }
    };
    let computer = if computer.is_empty() || computer == "self" {
        "local"
    } else {
        computer
    };
    let mut changed = false;
    let mut newly_reviewing: Vec<(String, String)> = Vec::new();
    if let Some(runs) = payload.get("runs").and_then(|v| v.as_array()) {
        for raw in runs {
            if let Some(pair) = merge_gateway_run(s, computer, raw) {
                changed = true;
                if let Some(rev_pair) = pair {
                    newly_reviewing.push(rev_pair);
                }
            }
        }
    } else if let Some(single) = payload.get("run") {
        if let Some(pair) = merge_gateway_run(s, computer, single) {
            changed = true;
            if let Some(rev_pair) = pair {
                newly_reviewing.push(rev_pair);
            }
        }
    }

    if let Some((pending_asid, token)) = pending_start {
        let run_id = payload
            .pointer("/run/id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let coder_asid = payload
            .pointer("/coder/asid")
            .or_else(|| payload.pointer("/run/state/coder_asid"))
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        if let Some(ref id) = run_id {
            s.orchestration.selected_run = Some(id.clone());
        }
        if let Some(real_asid) = coder_asid {
            let agent_id = payload
                .pointer("/coder/agent")
                .or_else(|| payload.pointer("/run/request/coordinator"))
                .and_then(|v| v.as_str())
                .unwrap_or(&s.start_agent)
                .to_string();
            let dir = payload
                .pointer("/run/request/cwd")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let goal = payload
                .pointer("/run/request/goal")
                .and_then(|v| v.as_str());
            adopt_gateway_coder_session(s, &pending_asid, token, &real_asid, &agent_id, dir, goal);
        } else {
            // Gateway accepted the orchestration request, even if the coder session
            // binding is not yet in this response (e.g. LocalHub or async binding).
            s.echo_submitted(token, true);
            s.echo_rows(&pending_asid);
            s.status = "Running…".into();
            if let Some(id) = run_id {
                s.orchestration
                    .pending_start_runs
                    .insert(id.clone(), (pending_asid, token));
                request_gateway_read(s, &id);
            }
            s.sync_watch();
        }
    }

    // Complete deferred session adoption for accepted starts once a subsequent Read/List
    // delivers the canonical coder_asid.
    struct DeferredCoderAdoption {
        run_id: String,
        pending_asid: String,
        token: u64,
        real_asid: String,
        agent_id: String,
        dir: Option<String>,
        goal: String,
    }
    let resolved_pending: Vec<DeferredCoderAdoption> = s
        .orchestration
        .pending_start_runs
        .iter()
        .filter_map(|(run_id, (pending_asid, token))| {
            let run = s.orchestration.runs.iter().find(|r| &r.id == run_id)?;
            if run.coder_asid.is_empty() || run.coder_asid.starts_with("new-") {
                return None;
            }
            Some(DeferredCoderAdoption {
                run_id: run_id.clone(),
                pending_asid: pending_asid.clone(),
                token: *token,
                real_asid: run.coder_asid.clone(),
                agent_id: run.coder.agent.clone(),
                dir: (!run.cwd.is_empty()).then(|| run.cwd.clone()),
                goal: run.goal.clone(),
            })
        })
        .collect();
    for item in resolved_pending {
        s.orchestration.pending_start_runs.remove(&item.run_id);
        adopt_gateway_coder_session(
            s,
            &item.pending_asid,
            item.token,
            &item.real_asid,
            &item.agent_id,
            item.dir,
            Some(&item.goal),
        );
    }

    for (coder_asid, rev_asid) in newly_reviewing {
        s.remember_subagent_context(&rev_asid, Some(&coder_asid));
        s.timelines.entry(rev_asid.clone()).or_default();
        if s.mode == Mode::Session
            && s.selected.as_deref() == Some(coder_asid.as_str())
            && coder_asid != rev_asid
        {
            s.open_detail(&coder_asid, &rev_asid);
        }
    }

    if changed {
        s.orchestration.runs.sort_by_key(|r| r.created_ms);
        if s.orchestration.selected_run.is_none() {
            s.orchestration.selected_run = s.orchestration.runs.last().map(|r| r.id.clone());
        }
        s.sync_watch();
        persist_runs(s);
    }
}

fn parse_gateway_model(model: Option<&str>, variant: Option<&str>) -> Option<ModelRef> {
    let model = model.map(str::trim).filter(|m| !m.is_empty())?;
    let (provider_id, model_id) = match model.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => (String::new(), model.to_string()),
    };
    Some(ModelRef {
        provider_id,
        model_id,
        variant: variant
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
    })
}

fn merge_gateway_run(
    s: &mut UiState,
    computer: &str,
    raw: &serde_json::Value,
) -> Option<Option<(String, String)>> {
    let id = raw.get("id").and_then(|v| v.as_str())?;
    let req = raw.get("request")?;
    let rev = req.get("reviewer").filter(|v| !v.is_null())?;
    let goal = req
        .get("goal")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let cwd = req
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let st = raw.get("state");
    let active_coder = st
        .and_then(|v| v.get("active_coder"))
        .filter(|v| !v.is_null());
    let active_reviewer = st
        .and_then(|v| v.get("active_reviewer"))
        .filter(|v| !v.is_null());
    let coder = if let Some(ac) = active_coder {
        // Read every field from `active_coder` exclusively so an agent-default
        // failover entry never inherits the original coder's model/variant/machine.
        MaoRole {
            agent: ac
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("codex")
                .to_string(),
            model: parse_gateway_model(
                ac.get("model").and_then(|v| v.as_str()),
                ac.get("variant").and_then(|v| v.as_str()),
            ),
            machine: ac
                .get("machine")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }
    } else {
        MaoRole {
            agent: req
                .get("coordinator")
                .and_then(|v| v.as_str())
                .unwrap_or("codex")
                .to_string(),
            model: parse_gateway_model(
                req.get("model").and_then(|v| v.as_str()),
                req.get("variant").and_then(|v| v.as_str()),
            ),
            machine: req
                .get("machine")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }
    };
    let eff_rev = active_reviewer.unwrap_or(rev);
    let reviewer = MaoRole {
        agent: eff_rev
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("codex")
            .to_string(),
        model: parse_gateway_model(
            eff_rev.get("model").and_then(|v| v.as_str()),
            eff_rev.get("variant").and_then(|v| v.as_str()),
        ),
        machine: eff_rev
            .get("machine")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    };
    let created_ms = raw.get("created_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    let round = st
        .and_then(|v| v.get("round"))
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 5) as u8;
    let max_rounds = st
        .and_then(|v| v.get("max_rounds"))
        .and_then(|v| v.as_u64())
        .or_else(|| req.get("max_review_rounds").and_then(|v| v.as_u64()))
        .unwrap_or(2)
        .clamp(1, 5) as u8;
    let coder_asid = st
        .and_then(|v| v.get("coder_asid"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let reviewer_asid = st
        .and_then(|v| v.get("reviewer_asid"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let base_commit = st
        .and_then(|v| v.get("base_commit"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let base_commits: std::collections::BTreeMap<String, String> = st
        .and_then(|v| v.get("base_commits"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let coder_summary = st
        .and_then(|v| v.get("coder_summary"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let review_summary = st
        .and_then(|v| v.get("review_summary"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let mao_phase = st
        .and_then(|v| v.get("mao_phase"))
        .and_then(|v| v.as_str())
        .unwrap_or("coding");
    let phase = match mao_phase {
        "approved" => MaoPhase::Approved {
            round,
            summary: review_summary.clone(),
        },
        "reviewing" => MaoPhase::Reviewing { round },
        "changes_requested" => MaoPhase::ChangesRequested {
            round,
            feedback: review_summary.clone(),
        },
        "failed" => MaoPhase::Failed {
            error: if review_summary.is_empty() {
                "Failed".into()
            } else {
                review_summary.clone()
            },
        },
        "cancelled" => MaoPhase::Cancelled,
        _ => MaoPhase::Coding { round },
    };
    let files: Vec<MaoDeliverableFile> = st
        .and_then(|v| v.get("files"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    Some(MaoDeliverableFile {
                        path: f.get("path")?.as_str()?.to_string(),
                        additions: f.get("additions").and_then(|v| v.as_u64()).unwrap_or(0)
                            as usize,
                        deletions: f.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0)
                            as usize,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let findings: Vec<MaoFinding> = st
        .and_then(|v| v.get("findings"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    Some(MaoFinding {
                        severity: f
                            .get("severity")
                            .and_then(|v| v.as_str())
                            .unwrap_or("medium")
                            .to_string(),
                        location: f
                            .get("location")
                            .and_then(|v| v.as_str())
                            .unwrap_or("workspace")
                            .to_string(),
                        message: f.get("message")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let steps: Vec<MaoStep> = st
        .and_then(|v| v.get("steps"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|step| {
                    let kind = match step
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("coding")
                    {
                        "review" => MaoStepKind::Review,
                        "rework" => MaoStepKind::Rework,
                        "verdict" => MaoStepKind::Verdict,
                        "failover" => MaoStepKind::Failover,
                        "system" => MaoStepKind::System,
                        _ => MaoStepKind::Coding,
                    };
                    let status = match step
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("running")
                    {
                        "completed" | "done" => "completed",
                        "warning" => "warning",
                        "failed" => "failed",
                        "cancelled" => "cancelled",
                        _ => "running",
                    };
                    Some(MaoStep {
                        kind,
                        round: step.get("round").and_then(|v| v.as_u64()).unwrap_or(1) as u8,
                        title: step.get("title")?.as_str()?.to_string(),
                        detail: step
                            .get("detail")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        status,
                        asid: step
                            .get("asid")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        time_ms: step
                            .get("time_ms")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(created_ms),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let updated_ms = steps
        .last()
        .map(|step| step.time_ms)
        .unwrap_or(created_ms)
        .max(created_ms);

    let gw_run = MaoRun {
        id: id.to_string(),
        computer: computer.to_string(),
        goal: goal.clone(),
        cwd,
        coder,
        reviewer,
        coder_asid: coder_asid.clone(),
        reviewer_asid,
        pending_reviewer_token: None,
        pending_review_diff: false,
        pre_review_msg_id: None,
        base_commit,
        base_commits,
        last_diff_fingerprint: None,
        snapshot_retries: 0,
        round,
        max_rounds,
        phase,
        files,
        coder_summary,
        review_summary,
        findings,
        steps,
        expanded: false,
        created_ms,
        updated_ms,
    };

    let rev_pair_if_local = (matches!(gw_run.phase, MaoPhase::Reviewing { .. })
        && gw_run.reviewer.machine.is_none()
        && !gw_run.coder_asid.is_empty())
    .then(|| {
        gw_run
            .reviewer_asid
            .as_ref()
            .filter(|rev_id| !rev_id.is_empty())
            .map(|rev_id| (gw_run.coder_asid.clone(), rev_id.clone()))
    })
    .flatten();

    if !coder_asid.is_empty() {
        if let Some(orig) = st
            .and_then(|v| v.get("original_coder_title"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != coder_asid)
        {
            s.orchestration
                .preserved_titles
                .entry(coder_asid.clone())
                .or_insert_with(|| orig.to_string());
            if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == coder_asid) {
                info.title = s
                    .orchestration
                    .preserved_titles
                    .get(&coder_asid)
                    .cloned()
                    .unwrap_or_else(|| orig.to_string());
            }
        }
    }
    if !coder_asid.is_empty()
        && gw_run.is_active()
        && s.cfg.ui.mao.session_overrides.get(&coder_asid) != Some(&false)
    {
        s.orchestration.enabled_sessions.insert(coder_asid.clone());
    }

    if let Some(existing) = s.orchestration.runs.iter_mut().find(|r| {
        r.id == id
            || (!coder_asid.is_empty()
                && r.computer == computer
                && r.coder_asid == coder_asid
                && r.goal == goal)
    }) {
        if existing.steps.len() > gw_run.steps.len() && existing.is_active() {
            return None;
        }
        let was_same_rev = matches!(existing.phase, MaoPhase::Reviewing { .. })
            && existing.reviewer_asid == gw_run.reviewer_asid;
        *existing = MaoRun {
            id: existing.id.clone(),
            expanded: existing.expanded,
            ..gw_run
        };
        return Some(if was_same_rev {
            None
        } else {
            rev_pair_if_local
        });
    }

    s.orchestration.runs.push(gw_run);
    Some(rev_pair_if_local)
}

// ---- Tests --------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands::Command;
    use muqun_gateway::platform::discovery::{AgentAvailability, AgentFeatures};

    fn test_agent(id: &str, name: &str) -> AgentDiscoveryInfo {
        AgentDiscoveryInfo {
            id: id.into(),
            name: name.into(),
            kind: "native".into(),
            status: AgentAvailability::Connected,
            enabled: true,
            version: None,
            reason: None,
            endpoint: None,
            models: vec![],
            modes: vec![],
            features: AgentFeatures {
                streaming: true,
                reasoning_effort: true,
                model_selection: true,
                tool_approvals: true,
                worktrees: true,
                revert: true,
                staged_revert: false,
                inbox: true,
                modes: false,
                skills: true,
                slash_commands: false,
                compaction: false,
                background_shells: true,
                attachments: true,
                attachments_by_path: false,
                extra: Default::default(),
            },
        }
    }

    fn test_session(asid: &str, agent_id: &str, title: &str) -> AgentSessionInfo {
        serde_json::from_value(serde_json::json!({
            "asid": asid,
            "agent_id": agent_id,
            "title": title,
            "directory": "/tmp/repo",
            "status": "busy",
            "updated_ms": 1
        }))
        .unwrap()
    }

    fn test_text_item(
        id: &str,
        message_id: &str,
        text: &str,
    ) -> muqun_gateway::agents::TimelineItem {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "message_id": message_id,
            "role": "assistant",
            "part": { "type": "text", "text": text },
            "seq": 1,
            "updated_ms": 3,
            "ordinal": 0
        }))
        .unwrap()
    }

    #[test]
    fn verdict_parser_respects_explicit_last_marker_and_fallbacks() {
        assert_eq!(
            parse_review_verdict("Looks good.\nMAO_VERDICT: APPROVED"),
            ReviewVerdict::Approved {
                summary: "Looks good.".into()
            }
        );
        assert!(matches!(
            parse_review_verdict(
                "Note: MAO_VERDICT: APPROVED was mentioned in template, however null deref found.\nMAO_VERDICT: CHANGES_REQUESTED"
            ),
            ReviewVerdict::ChangesRequested { .. }
        ));
        assert!(matches!(
            parse_review_verdict("存在问题：缺少边界判断，建议修改"),
            ReviewVerdict::ChangesRequested { .. }
        ));
    }

    #[test]
    fn low_severity_only_findings_auto_approve_without_rework_loop() {
        let reply = "\
Overall implementation looks solid.\n\
- MAO_ISSUE: [LOW] src/lib.rs:12 | Minor docstring typo\n\
MAO_VERDICT: CHANGES_REQUESTED";

        let findings = parse_review_findings(reply);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, "low");
        assert!(matches!(
            parse_review_verdict(reply),
            ReviewVerdict::Approved { .. }
        ));
    }

    #[test]
    fn stagnant_rework_diff_short_circuits_ping_pong_loop() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.agents = vec![test_agent("claude", "Claude"), test_agent("codex", "Codex")];
        s.cfg.ui.mao.enabled = true;
        s.cfg.ui.mao.max_rounds = 3;

        on_start_submitted(&mut s, "new-1", "Fix handler", "/tmp/repo", "claude", None);
        on_session_created(&mut s, 1, &test_session("coder-1", "claude", "Fix handler"));
        on_session_status(&mut s, "coder-1", AgentSessionStatus::Idle);
        let run_id = s.orchestration.runs[0].id.clone();

        on_review_ready(
            &mut s,
            &run_id,
            "M src/lib.rs".into(),
            vec![("src/lib.rs".into(), 10, 2)],
        );
        let rev_token = s.orchestration.runs[0].pending_reviewer_token.unwrap();
        on_session_created(&mut s, rev_token, &test_session("rev-1", "codex", "Review"));
        s.timelines
            .entry("rev-1".into())
            .or_default()
            .replace(vec![test_text_item(
                "r1",
                "m1",
                "Handle edge case.\nMAO_ISSUE: [HIGH] src/lib.rs:5 | Missing check\nMAO_VERDICT: CHANGES_REQUESTED",
            )]);
        on_session_status(&mut s, "rev-1", AgentSessionStatus::Idle);
        assert_eq!(s.orchestration.runs[0].phase, MaoPhase::Coding { round: 2 });
        while rx.try_recv().is_ok() {}

        // Rework round produces the exact same diff -> must short-circuit without dispatching Reviewer again.
        on_session_status(&mut s, "coder-1", AgentSessionStatus::Idle);
        while rx.try_recv().is_ok() {}
        on_review_ready(
            &mut s,
            &run_id,
            "M src/lib.rs".into(),
            vec![("src/lib.rs".into(), 10, 2)],
        );

        assert!(matches!(
            s.orchestration.runs[0].phase,
            MaoPhase::ChangesRequested { round: 2, .. }
        ));
        while let Ok(cmd) = rx.try_recv() {
            assert!(
                !matches!(cmd, Command::Compose(ComposeCommand::Submit { .. })),
                "stagnant diff must not spawn another reviewer pass"
            );
        }
    }

    #[test]
    fn empty_reviewer_reply_watchdog_unstalls_after_retry() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.agents = vec![test_agent("claude", "Claude"), test_agent("codex", "Codex")];
        s.cfg.ui.mao.enabled = true;

        on_start_submitted(&mut s, "new-1", "Fix bug", "/tmp/repo", "claude", None);
        on_session_created(&mut s, 1, &test_session("coder-1", "claude", "Fix bug"));
        let run_id = s.orchestration.runs[0].id.clone();

        on_review_ready(
            &mut s,
            &run_id,
            "M src/lib.rs".into(),
            vec![("src/lib.rs".into(), 4, 1)],
        );
        let rev_token = s.orchestration.runs[0].pending_reviewer_token.unwrap();
        on_session_created(&mut s, rev_token, &test_session("rev-1", "codex", "Review"));
        while rx.try_recv().is_ok() {}

        // 1st empty idle -> requests snapshot retry.
        on_session_status(&mut s, "rev-1", AgentSessionStatus::Idle);
        assert_eq!(
            s.orchestration.runs[0].phase,
            MaoPhase::Reviewing { round: 1 }
        );
        assert_eq!(s.orchestration.runs[0].snapshot_retries, 1);

        // 2nd empty idle -> watchdog fail-open so pipeline never hangs in Reviewing.
        on_session_status(&mut s, "rev-1", AgentSessionStatus::Idle);
        assert!(matches!(
            s.orchestration.runs[0].phase,
            MaoPhase::ChangesRequested { round: 1, .. }
        ));
    }

    #[test]
    fn cheap_model_prefers_luna_and_serde_roundtrips_cleanly() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let mut codex = test_agent("codex", "Codex");
        codex.models = serde_json::from_value(serde_json::json!([
            {"id": "gpt-6.2-astra", "name": "GPT-6.2-Astra", "providerId": "openai", "supportsReasoning": true, "reasoningEffortTiers": ["low", "high"]},
            {"id": "gpt-6.1-terra", "name": "GPT-6.1 Terra", "providerId": "openai", "supportsReasoning": true, "reasoningEffortTiers": ["low", "medium"]},
            {"id": "gpt-5.6-luna", "name": "GPT-5.6-Luna", "providerId": "openai", "supportsReasoning": true, "reasoningEffortTiers": ["low", "medium", "high"]}
        ]))
        .unwrap();
        s.agents = vec![codex];
        // Configure an unavailable legacy model -> must auto-fallback to gpt-5.6-luna / low.
        s.cfg.ui.mao = MaoPrefs {
            enabled: true,
            reviewer: Some(MaoRoleConfig {
                agent: "codex".into(),
                model: Some(ModelRef {
                    provider_id: "openai".into(),
                    model_id: "gpt-5.1-codex-mini".into(),
                    variant: Some("low".into()),
                }),
                machine: None,
            }),
            max_rounds: 2,
            ..Default::default()
        };

        let picked = preferred_cheap_model(&s, "codex").expect("cheap model");
        assert_eq!(picked.model_id, "gpt-5.6-luna");
        assert_eq!(picked.variant.as_deref(), Some("low"));

        let eff = effective_reviewer(&s, "claude");
        assert_eq!(eff.model.as_ref().unwrap().model_id, "gpt-5.6-luna");

        on_start_submitted(&mut s, "new-1", "Persist check", "/tmp/repo", "codex", None);
        let json = serde_json::to_string(&s.orchestration.runs).unwrap();
        let decoded: Vec<MaoRun> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, s.orchestration.runs);
    }

    #[test]
    fn parses_structured_findings_and_strips_verdict_markers() {
        let reply = "\
Proxy validation still trusts the left-most forwarded header.\n\
- MAO_ISSUE: [HIGH] src/proxy.rs:88 | Validate trusted proxy hops right-to-left\n\
* MAO_ISSUE: [LOW] src/proxy_test.rs:14 | Add loopback test\n\
MAO_VERDICT: CHANGES_REQUESTED";

        let findings = parse_review_findings(reply);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].severity, "high");
        assert_eq!(findings[0].location, "src/proxy.rs:88");
        assert_eq!(
            findings[0].message,
            "Validate trusted proxy hops right-to-left"
        );
        assert_eq!(findings[1].severity, "low");

        let verdict = parse_review_verdict(reply);
        match verdict {
            ReviewVerdict::ChangesRequested { feedback } => {
                assert!(!feedback.contains("MAO_VERDICT:"));
                assert!(!feedback.contains("MAO_ISSUE:"));
                assert!(feedback.contains("left-most forwarded header"));
            }
            _ => panic!("expected changes requested"),
        }
    }

    #[test]
    fn skips_reviewer_when_coder_produces_no_code_changes() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.agents = vec![test_agent("claude", "Claude"), test_agent("codex", "Codex")];
        s.cfg.ui.mao.enabled = true;

        on_start_submitted(
            &mut s,
            "new-1",
            "Explain architecture",
            "/tmp/repo",
            "claude",
            None,
        );
        on_session_created(
            &mut s,
            1,
            &test_session("coder-1", "claude", "Explain architecture"),
        );
        on_session_status(&mut s, "coder-1", AgentSessionStatus::Idle);
        while rx.try_recv().is_ok() {}

        let run_id = s.orchestration.runs[0].id.clone();
        on_review_ready(&mut s, &run_id, String::new(), Vec::new());

        assert!(matches!(
            s.orchestration.runs[0].phase,
            MaoPhase::Approved { round: 1, .. }
        ));
        assert!(s.orchestration.runs[0].reviewer_asid.is_none());
        while let Ok(cmd) = rx.try_recv() {
            assert!(
                !matches!(cmd, Command::Compose(ComposeCommand::Submit { .. })),
                "reviewer must not be dispatched on empty diff"
            );
        }
    }

    #[test]
    fn merges_gateway_orchestration_runs_from_hub_update() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let payload = serde_json::json!({
            "runs": [{
                "id": "gw-run-1",
                "owner": "dev-1",
                "created_ms": 1700000000000u64,
                "request": {
                    "client_ref": "c1",
                    "goal": "Fix webhook retry backoff",
                    "cwd": "/repo",
                    "coordinator": "claude",
                    "model": "anthropic/claude-haiku-4-5",
                    "variant": "low",
                    "reviewer": {
                        "agent": "codex",
                        "model": "openai/gpt-5.1-codex-mini",
                        "variant": "low"
                    },
                    "max_review_rounds": 2
                },
                "state": {
                    "round": 1,
                    "max_rounds": 2,
                    "mao_phase": "changes_requested",
                    "accepted": false,
                    "coder_asid": "coder-s1",
                    "reviewer_asid": "rev-s1",
                    "base_commit": "abc1234",
                    "files": [{"path": "src/webhook.rs", "additions": 12, "deletions": 3}],
                    "coder_summary": "Added capped exponential backoff.",
                    "review_summary": "Cap max retry delay at 30s.",
                    "findings": [{
                        "severity": "high",
                        "location": "src/webhook.rs:51",
                        "message": "Cap backoff delay to avoid overflow"
                    }],
                    "steps": [{
                        "kind": "review",
                        "round": 1,
                        "title": "Code review #1 · codex",
                        "detail": "Cap max retry delay at 30s.",
                        "status": "warning",
                        "asid": "rev-s1",
                        "time_ms": 1700000005000u64
                    }]
                }
            }]
        });

        on_gateway_orchestration(&mut s, "local", 1, Ok(payload));
        assert_eq!(s.orchestration.runs.len(), 1);
        let run = &s.orchestration.runs[0];
        assert_eq!(run.id, "gw-run-1");
        assert_eq!(run.files.len(), 1);
        assert_eq!(run.findings.len(), 1);
        assert_eq!(run.findings[0].severity, "high");
        assert!(matches!(
            run.phase,
            MaoPhase::ChangesRequested { round: 1, .. }
        ));
    }

    #[test]
    fn full_coder_review_rework_approved_loop() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.agents = vec![test_agent("claude", "Claude"), test_agent("codex", "Codex")];
        s.cfg.ui.mao = MaoPrefs {
            enabled: true,
            reviewer: Some(MaoRoleConfig {
                agent: "codex".into(),
                model: Some(ModelRef {
                    provider_id: "openai".into(),
                    model_id: "gpt-5.3-codex".into(),
                    variant: Some("high".into()),
                }),
                machine: None,
            }),
            max_rounds: 2,
            ..Default::default()
        };

        // 1. User submits from Start composer -> pending run created.
        let pending = "new-1";
        on_start_submitted(
            &mut s,
            pending,
            "Implement feature X",
            "/tmp/repo",
            "claude",
            None,
        );
        assert_eq!(s.orchestration.runs.len(), 1);
        assert_eq!(s.orchestration.runs[0].phase, MaoPhase::Coding { round: 1 });

        // 2. Real Coder session created -> bound to run.
        on_session_created(
            &mut s,
            1,
            &test_session("coder-1", "claude", "Implement feature X"),
        );
        assert_eq!(s.orchestration.runs[0].coder_asid, "coder-1");

        // 3. Coder completes round 1 -> MaoPrepareReview sent.
        on_session_status(&mut s, "coder-1", AgentSessionStatus::Idle);
        let mut saw_prepare = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::MaoPrepareReview { coder_asid, .. } = cmd {
                assert_eq!(coder_asid, "coder-1");
                saw_prepare = true;
            }
        }
        assert!(saw_prepare);
        let run_id = s.orchestration.runs[0].id.clone();

        // 4. Diff ready -> spawns Reviewer session and captures changed files.
        on_review_ready(
            &mut s,
            &run_id,
            "M src/lib.rs".into(),
            vec![("src/lib.rs".into(), 18, 4)],
        );
        assert_eq!(s.orchestration.runs[0].files.len(), 1);
        assert_eq!(s.orchestration.runs[0].files[0].additions, 18);
        assert_eq!(
            s.orchestration.runs[0].phase,
            MaoPhase::Reviewing { round: 1 }
        );
        let rev_token = s.orchestration.runs[0].pending_reviewer_token.unwrap();
        let mut saw_rev_submit = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Compose(ComposeCommand::Submit {
                target: Target::New { agent, model, .. },
                ..
            }) = cmd
            {
                assert_eq!(agent, "codex");
                assert_eq!(
                    model.as_ref().and_then(|m| m.variant.as_deref()),
                    Some("high")
                );
                saw_rev_submit = true;
            }
        }
        assert!(saw_rev_submit);

        // 5. Reviewer session created -> linked cleanly.
        on_session_created(&mut s, rev_token, &test_session("rev-1", "codex", "Review"));
        assert_eq!(
            s.orchestration.runs[0].reviewer_asid.as_deref(),
            Some("rev-1")
        );

        // 6. Reviewer finishes round 1 requesting changes -> automatic Rework Round 2.
        s.timelines
            .entry("rev-1".into())
            .or_default()
            .replace(vec![test_text_item(
                "t1",
                "m1",
                "Missing error check.\nMAO_VERDICT: CHANGES_REQUESTED",
            )]);
        on_session_status(&mut s, "rev-1", AgentSessionStatus::Idle);
        assert_eq!(s.orchestration.runs[0].phase, MaoPhase::Coding { round: 2 });
        let mut saw_rework_submit = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Compose(ComposeCommand::Submit {
                target: Target::Session(asid),
                ..
            }) = cmd
            {
                assert_eq!(asid.0, "coder-1");
                saw_rework_submit = true;
            }
        }
        assert!(saw_rework_submit);

        // 7. Coder finishes round 2 -> diff ready -> Reviewer round 2 -> APPROVED!
        on_session_status(&mut s, "coder-1", AgentSessionStatus::Idle);
        on_review_ready(
            &mut s,
            &run_id,
            "M src/lib.rs (fixed)".into(),
            vec![("src/lib.rs".into(), 22, 5)],
        );
        assert_eq!(
            s.orchestration.runs[0].phase,
            MaoPhase::Reviewing { round: 2 }
        );

        s.timelines
            .entry("rev-1".into())
            .or_default()
            .replace(vec![test_text_item(
                "t2",
                "m2",
                "All checks pass.\nMAO_VERDICT: APPROVED",
            )]);
        on_session_status(&mut s, "rev-1", AgentSessionStatus::Idle);
        assert!(matches!(
            s.orchestration.runs[0].phase,
            MaoPhase::Approved { round: 2, .. }
        ));
    }

    #[test]
    fn session_scope_does_not_hijack_unrelated_existing_sessions() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.agents = vec![test_agent("claude", "Claude"), test_agent("codex", "Codex")];
        s.cfg.ui.mao.enabled = true;
        s.selected = Some("existing-unrelated-1".into());

        // Start composer has MAO enabled, but an unrelated existing session is not automatically hijacked.
        assert!(is_slot_enabled(&s, Slot::Start));
        assert!(!is_slot_enabled(&s, Slot::Session));
        on_session_submitted(&mut s, "existing-unrelated-1", "Quick question");
        assert!(s.orchestration.runs.is_empty());

        // Explicitly toggling MAO inside that session enables it for that session only.
        toggle_enabled(&mut s, Slot::Session);
        assert!(is_slot_enabled(&s, Slot::Session));
        on_session_submitted(&mut s, "existing-unrelated-1", "Implement feature Y");
        assert_eq!(s.orchestration.runs.len(), 1);
        assert_eq!(s.orchestration.runs[0].coder_asid, "existing-unrelated-1");
    }

    #[test]
    fn gateway_dispatch_start_and_cross_machine_reviewer_control_actions() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.hub_label = "Local Gateway".into();
        s.agents = vec![test_agent("claude", "Claude"), test_agent("codex", "Codex")];
        s.cfg.ui.mao = MaoPrefs {
            enabled: true,
            reviewer: Some(MaoRoleConfig {
                agent: "codex".into(),
                model: Some(ModelRef {
                    provider_id: "openai".into(),
                    model_id: "gpt-5.6-luna".into(),
                    variant: Some("low".into()),
                }),
                machine: Some("mac-studio".into()),
            }),
            max_rounds: 3,
            ..Default::default()
        };
        s.pending_start = Some(("new-7".into(), 7));
        s.selected = Some("new-7".into());
        s.echo_sent("new-7", 7, "Cross-machine refactor".into(), &[], None);

        assert!(dispatch_gateway_start(
            &mut s,
            "new-7",
            7,
            "Cross-machine refactor",
            "/tmp/repo",
            "claude",
            None,
            None,
            None,
            &[],
        ));

        let mut start_ticket = None;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Orchestration {
                ticket,
                request: crate::hub::OrchestrationRequest::Start(body),
                ..
            } = cmd
            {
                assert_eq!(body["coordinator"], "claude");
                assert_eq!(body["auto_approve"], true);
                assert_eq!(body["reviewer"]["agent"], "codex");
                assert_eq!(body["reviewer"]["machine"], "mac-studio");
                assert_eq!(body["max_review_rounds"], 3);
                start_ticket = Some(ticket);
            }
        }
        let ticket = start_ticket.expect("gateway orchestration start dispatched");

        // Simulate Gateway response adopting the created Coder session.
        on_gateway_orchestration(
            &mut s,
            "local",
            ticket,
            Ok(serde_json::json!({
                "run": {
                    "id": "o_gw_99",
                    "owner": "local",
                    "created_ms": 1700000000000u64,
                    "request": {
                        "client_ref": "c99",
                        "goal": "Cross-machine refactor",
                        "cwd": "/tmp/repo",
                        "coordinator": "claude",
                        "reviewer": {
                            "agent": "codex",
                            "model": "openai/gpt-5.6-luna",
                            "variant": "low",
                            "machine": "mac-studio"
                        },
                        "max_review_rounds": 3
                    },
                    "state": {
                        "round": 1,
                        "max_rounds": 3,
                        "mao_phase": "coding",
                        "accepted": false,
                        "coder_asid": "coder-gw-99",
                        "steps": []
                    }
                },
                "coder": { "agent": "claude", "asid": "coder-gw-99" }
            })),
        );
        assert_eq!(s.selected.as_deref(), Some("coder-gw-99"));
        assert!(s.orchestration.is_session_enabled("coder-gw-99"));
        assert_eq!(
            s.orchestration.runs[0].reviewer.machine.as_deref(),
            Some("mac-studio")
        );
        // Echo must leave `Sending` immediately once Gateway accepts the orchestration.
        assert_eq!(
            s.echoes
                .get("coder-gw-99")
                .and_then(|list| list.first())
                .map(|e| e.state),
            Some(super::super::echo::EchoState::Sent)
        );
        // Wrapped provider user row reconciles and drops the optimistic echo cleanly.
        s.timelines
            .entry("coder-gw-99".into())
            .or_default()
            .replace(vec![serde_json::from_value(serde_json::json!({
                "id": "u1",
                "message_id": "m_u1",
                "role": "user",
                "part": {
                    "type": "text",
                    "text": "Implement and verify the user goal carefully. Honor repository instructions, run relevant checks/tests to confirm your changes work cleanly, and finish with changed files, commands executed, and results.\n\nUser goal:\nCross-machine refactor\n\nYou were started for a Muqun task: call tasks.complete."
                },
                "seq": 1,
                "updated_ms": 2,
                "ordinal": 0
            }))
            .unwrap()]);
        s.echo_rows("coder-gw-99");
        assert!(
            s.echoes
                .get("coder-gw-99")
                .is_none_or(|list| list.is_empty()),
            "optimistic echo should reconcile against wrapped orchestration prompt"
        );

        // Control actions on o_* runs forward to Gateway endpoints (with optional extra rework note).
        s.orchestration.rework_note = TextEdit::new("Also add a regression unit test.");
        s.orchestration.rework_note_run = Some("o_gw_99".into());
        continue_rework(&mut s, "o_gw_99");
        assert!(s.orchestration.rework_note.text.is_empty());
        mark_approved(&mut s, "o_gw_99");
        cancel_run(&mut s, "o_gw_99");
        let mut saw = (false, false, false);
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Orchestration { request, .. } = cmd {
                match request {
                    crate::hub::OrchestrationRequest::Rework { run, feedback }
                        if run == "o_gw_99" =>
                    {
                        assert!(feedback
                            .as_deref()
                            .is_some_and(|f| f.contains("regression unit test")));
                        saw.0 = true;
                    }
                    crate::hub::OrchestrationRequest::Approve(run) if run == "o_gw_99" => {
                        saw.1 = true;
                    }
                    crate::hub::OrchestrationRequest::Cancel(run) if run == "o_gw_99" => {
                        saw.2 = true;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(saw, (true, true, true));

        // Active Gateway run polling emits Read(id) and throttles rapid repeats.
        s.orchestration.runs[0].phase = MaoPhase::Reviewing { round: 1 };
        s.orchestration.last_gateway_poll_ms = 0;
        poll_active_gateway_runs(&mut s, false);
        let mut reads = 0usize;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Orchestration {
                request: crate::hub::OrchestrationRequest::Read(id),
                ..
            } = cmd
            {
                assert_eq!(id, "o_gw_99");
                reads += 1;
            }
        }
        assert_eq!(reads, 1);
        poll_active_gateway_runs(&mut s, false);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn gateway_start_without_coder_asid_marks_echo_sent_and_adopts_on_followup_sync() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.hub_label = "Local Gateway".into();
        s.agents = vec![test_agent("claude", "Claude"), test_agent("codex", "Codex")];
        s.cfg.ui.mao.enabled = true;
        s.pending_start = Some(("new-11".into(), 11));
        s.selected = Some("new-11".into());
        s.echo_sent("new-11", 11, "Move ready cards to review".into(), &[], None);

        assert!(dispatch_gateway_start(
            &mut s,
            "new-11",
            11,
            "Move ready cards to review",
            "/tmp/repo",
            "claude",
            None,
            None,
            None,
            &[],
        ));
        let mut start_ticket = None;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Orchestration {
                ticket,
                request: crate::hub::OrchestrationRequest::Start(_),
                ..
            } = cmd
            {
                start_ticket = Some(ticket);
            }
        }
        let ticket = start_ticket.expect("start command dispatched");

        // LocalHub / async response without `coder.asid` must still mark echo Sent (not Failed)
        // and schedule a follow-up Read for deferred session adoption.
        on_gateway_orchestration(
            &mut s,
            "local",
            ticket,
            Ok(serde_json::json!({
                "run": {
                    "id": "o_local_11",
                    "owner": "local",
                    "created_ms": 1700000000000u64,
                    "request": {
                        "client_ref": "c11",
                        "goal": "Move ready cards to review",
                        "cwd": "/tmp/repo",
                        "coordinator": "claude",
                        "reviewer": { "agent": "codex" }
                    }
                },
                "phase": "working"
            })),
        );
        assert_eq!(
            s.echoes
                .get("new-11")
                .and_then(|list| list.first())
                .map(|e| e.state),
            Some(super::super::echo::EchoState::Sent)
        );
        assert!(s
            .orchestration
            .pending_start_runs
            .contains_key("o_local_11"));
        let mut saw_read = false;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Orchestration {
                request: crate::hub::OrchestrationRequest::Read(id),
                ..
            } = cmd
            {
                if id == "o_local_11" {
                    saw_read = true;
                }
            }
        }
        assert!(
            saw_read,
            "expected follow-up Read for deferred coder binding"
        );

        // Follow-up Read response delivers canonical coder_asid -> adopts session & migrates echo.
        on_gateway_orchestration(
            &mut s,
            "local",
            999,
            Ok(serde_json::json!({
                "run": {
                    "id": "o_local_11",
                    "owner": "local",
                    "created_ms": 1700000000000u64,
                    "request": {
                        "client_ref": "c11",
                        "goal": "Move ready cards to review",
                        "cwd": "/tmp/repo",
                        "coordinator": "claude",
                        "reviewer": { "agent": "codex" }
                    },
                    "state": {
                        "round": 1,
                        "max_rounds": 2,
                        "mao_phase": "coding",
                        "accepted": false,
                        "coder_asid": "coder-real-11",
                        "steps": []
                    }
                }
            })),
        );
        assert_eq!(s.selected.as_deref(), Some("coder-real-11"));
        assert!(!s
            .orchestration
            .pending_start_runs
            .contains_key("o_local_11"));
        assert_eq!(
            s.echoes
                .get("coder-real-11")
                .and_then(|list| list.first())
                .map(|e| e.state),
            Some(super::super::echo::EchoState::Sent)
        );

        // Canonical user row arrives -> echo reconciles cleanly.
        s.timelines
            .entry("coder-real-11".into())
            .or_default()
            .replace(vec![serde_json::from_value(serde_json::json!({
                "id": "u11",
                "message_id": "m_u11",
                "role": "user",
                "part": { "type": "text", "text": "Move ready cards to review" },
                "seq": 1,
                "updated_ms": 2,
                "ordinal": 0
            }))
            .unwrap()]);
        s.echo_rows("coder-real-11");
        assert!(s
            .echoes
            .get("coder-real-11")
            .is_none_or(|list| list.is_empty()));
    }

    #[test]
    fn conversational_followups_bypass_mao_orchestration_creation() {
        for q in [
            "你改了什么？ 为什么现在发消息明明发出去了还显示发送中",
            "还差哪些没有好的？ 你改了什么今天",
            "权限管理做哪里了",
            "那具体来说要怎样做？ 会不会把项目搞烂了",
            "每个消息都走一遍流程么？？",
            "what changed in this commit?",
        ] {
            assert!(
                is_conversational_query(q),
                "expected conversational query to bypass MAO: {q}"
            );
        }
        for cmd in [
            "修复 warm_agent 目录校验并补单测",
            "帮我把这个按钮改一下，改成双行布局",
            "把好了的card 移到 review里面",
            "Implement rate limiting for /api/search",
        ] {
            assert!(
                !is_conversational_query(cmd),
                "expected actionable task to remain eligible for MAO: {cmd}"
            );
        }
    }

    #[test]
    fn dispatch_gateway_start_includes_user_agent_pool_and_auto_failover() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.hub_label = "gateway".into();
        s.cfg.ui.mao.auto_failover = true;
        s.cfg.ui.mao.agent_pool = vec![
            MaoRoleConfig {
                agent: "claude".into(),
                model: Some(ModelRef {
                    provider_id: "anthropic".into(),
                    model_id: "claude-opus-4-6".into(),
                    variant: Some("high".into()),
                }),
                machine: None,
            },
            MaoRoleConfig {
                agent: "codex".into(),
                model: Some(ModelRef {
                    provider_id: "openai".into(),
                    model_id: "gpt-5.6".into(),
                    variant: None,
                }),
                machine: Some("mini".into()),
            },
        ];

        assert!(dispatch_gateway_start(
            &mut s,
            "new-99",
            99,
            "Refactor auth middleware",
            "/tmp",
            "claude",
            None,
            None,
            None,
            &[],
        ));
        let mut body = None;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Orchestration {
                request: crate::hub::OrchestrationRequest::Start(payload),
                ..
            } = cmd
            {
                body = Some(payload);
            }
        }
        let body = body.expect("expected orchestration start command");
        assert_eq!(body["auto_failover"], true);
        let pool = body["agent_pool"].as_array().expect("agent_pool array");
        assert_eq!(pool.len(), 2);
        assert_eq!(pool[0]["agent"], "claude");
        assert_eq!(pool[0]["model"], "anthropic/claude-opus-4-6");
        assert_eq!(pool[0]["variant"], "high");
        assert_eq!(pool[1]["agent"], "codex");
        assert_eq!(pool[1]["machine"], "mini");
    }

    #[test]
    fn merge_gateway_run_reflects_active_failover_coder_and_step() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let raw = serde_json::json!({
            "id": "o_fo_1",
            "created_ms": 1000,
            "request": {
                "goal": "Implement pool",
                "cwd": "/tmp",
                "coordinator": "claude",
                "reviewer": { "agent": "codex" }
            },
            "state": {
                "mao_phase": "coding",
                "round": 1,
                "max_rounds": 2,
                "coder_asid": "codex-fo-asid",
                "active_coder": { "agent": "codex", "model": "openai/gpt-5.6" },
                "failover_count": 1,
                "steps": [
                    {
                        "kind": "failover",
                        "round": 1,
                        "title": "Auto-switched Coder: claude → codex (openai/gpt-5.6)",
                        "detail": "rate limit reached",
                        "status": "warning",
                        "time_ms": 1001
                    }
                ]
            }
        });
        assert!(merge_gateway_run(&mut s, "local", &raw).is_some());
        let run = s.orchestration.runs.last().unwrap();
        assert_eq!(run.coder.agent, "codex");
        assert_eq!(
            run.coder.model.as_ref().map(|m| m.model_id.as_str()),
            Some("gpt-5.6")
        );
        assert_eq!(run.steps[0].kind, MaoStepKind::Failover);
    }

    #[test]
    fn session_mao_toggle_off_persists_across_gateway_sync_and_reopen() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.agents = vec![test_agent("claude", "Claude"), test_agent("codex", "Codex")];
        s.selected = Some("sess-keep-off".into());

        // Turn ON for this session -> persisted in session_overrides.
        toggle_enabled(&mut s, Slot::Session);
        assert!(is_slot_enabled(&s, Slot::Session));
        assert_eq!(
            s.cfg.ui.mao.session_overrides.get("sess-keep-off"),
            Some(&true)
        );

        // Turn OFF for this session -> persisted as false.
        toggle_enabled(&mut s, Slot::Session);
        assert!(!is_slot_enabled(&s, Slot::Session));
        assert_eq!(
            s.cfg.ui.mao.session_overrides.get("sess-keep-off"),
            Some(&false)
        );

        // Gateway syncs an active or historical run for this session -> must NOT re-enable it!
        let gw_run = serde_json::json!({
            "id": "o_hist_1",
            "created_ms": 1000,
            "request": {
                "goal": "Earlier task",
                "cwd": "/tmp",
                "coordinator": "claude",
                "reviewer": { "agent": "codex" }
            },
            "state": {
                "mao_phase": "coding",
                "round": 1,
                "max_rounds": 2,
                "coder_asid": "sess-keep-off",
                "steps": []
            }
        });
        assert!(merge_gateway_run(&mut s, "local", &gw_run).is_some());
        assert!(
            !is_slot_enabled(&s, Slot::Session),
            "Gateway sync must never flip a user-disabled session back to enabled"
        );

        // Switching to another session and coming back keeps it OFF.
        s.selected = Some("sess-other".into());
        assert!(!is_slot_enabled(&s, Slot::Session));
        s.selected = Some("sess-keep-off".into());
        assert!(!is_slot_enabled(&s, Slot::Session));
    }

    #[test]
    fn session_custom_mao_config_overrides_global_without_affecting_other_sessions() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.hub_label = "gateway".into();
        s.agents = vec![
            test_agent("claude", "Claude"),
            test_agent("codex", "Codex"),
            test_agent("antigravity", "Antigravity"),
        ];
        // Global defaults: reviewer = codex, max_rounds = 2, auto_failover = true.
        s.cfg.ui.mao.reviewer = Some(MaoRoleConfig {
            agent: "codex".into(),
            model: None,
            machine: None,
        });
        s.cfg.ui.mao.max_rounds = 2;
        s.cfg.ui.mao.auto_failover = true;

        // Both Session A and Session B inherit global defaults initially.
        assert_eq!(
            effective_reviewer_for(&s, "claude", Some("sess-a")).agent,
            "codex"
        );
        assert_eq!(effective_max_rounds(&s, Some("sess-a")), 2);
        assert!(effective_auto_failover(&s, Some("sess-a")));

        // Customize ONLY Session A via Slot::Session.
        s.selected = Some("sess-a".into());
        set_reviewer_agent(&mut s, MaoMenuAnchor::SessionComposer, "antigravity".into());
        set_max_rounds(&mut s, MaoMenuAnchor::SessionComposer, 4);
        toggle_auto_failover(&mut s, MaoMenuAnchor::SessionComposer);

        // Session A uses its own overrides; global & Session B are untouched!
        assert!(has_session_config_override(&s, Some("sess-a")));
        assert!(!has_session_config_override(&s, Some("sess-b")));
        assert_eq!(
            effective_reviewer_for(&s, "claude", Some("sess-a")).agent,
            "antigravity"
        );
        assert_eq!(effective_max_rounds(&s, Some("sess-a")), 4);
        assert!(!effective_auto_failover(&s, Some("sess-a")));

        assert_eq!(
            s.cfg.ui.mao.reviewer.as_ref().map(|r| r.agent.as_str()),
            Some("codex")
        );
        assert_eq!(s.cfg.ui.mao.max_rounds, 2);
        assert!(s.cfg.ui.mao.auto_failover);
        assert_eq!(
            effective_reviewer_for(&s, "claude", Some("sess-b")).agent,
            "codex"
        );
        assert_eq!(effective_max_rounds(&s, Some("sess-b")), 2);
        assert!(effective_auto_failover(&s, Some("sess-b")));

        // Gateway dispatch for Session A carries Session A's custom config.
        assert!(dispatch_gateway_start(
            &mut s,
            "sess-a",
            101,
            "Task on A",
            "/tmp",
            "claude",
            None,
            None,
            Some("sess-a"),
            &[],
        ));
        let mut body_a = None;
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Orchestration {
                request: crate::hub::OrchestrationRequest::Start(payload),
                ..
            } = cmd
            {
                body_a = Some(payload);
            }
        }
        let body_a = body_a.expect("dispatch payload for A");
        assert_eq!(body_a["reviewer"]["agent"], "antigravity");
        assert_eq!(body_a["max_review_rounds"], 4);
        assert_eq!(body_a["auto_failover"], false);

        // Resetting Session A restores global inheritance.
        reset_session_mao_config(&mut s, MaoMenuAnchor::SessionComposer);
        assert!(!has_session_config_override(&s, Some("sess-a")));
        assert_eq!(
            effective_reviewer_for(&s, "claude", Some("sess-a")).agent,
            "codex"
        );
        assert_eq!(effective_max_rounds(&s, Some("sess-a")), 2);
        assert!(effective_auto_failover(&s, Some("sess-a")));
    }

    #[test]
    fn agent_default_failover_does_not_leak_original_model_and_adopts_fallback_session() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.hub_label = "gateway".into();
        s.selected = Some("existing-claude-1".into());
        s.echo_sent("existing-claude-1", 44, "Fix bug".into(), &[], None);
        s.orchestration
            .gateway_start_tokens
            .insert(700, ("existing-claude-1".into(), 44));

        // Pre-dispatch failover switched from claude (with explicit model) to codex (agent default).
        let payload = serde_json::json!({
            "run": {
                "id": "o_fo_default_1",
                "created_ms": 2000,
                "request": {
                    "goal": "Fix bug",
                    "cwd": "/tmp",
                    "coordinator": "claude",
                    "model": "anthropic/claude-opus-4-6",
                    "variant": "high",
                    "machine": "mini",
                    "reviewer": { "agent": "antigravity" }
                },
                "state": {
                    "mao_phase": "coding",
                    "round": 1,
                    "max_rounds": 2,
                    "coder_asid": "codex-fallback-2",
                    "active_coder": { "agent": "codex" },
                    "failover_count": 1,
                    "steps": []
                }
            },
            "coder": {
                "agent": "codex",
                "asid": "codex-fallback-2"
            }
        });
        on_gateway_orchestration(&mut s, "local", 700, Ok(payload));

        // 1. Coder role must NOT inherit claude's model/variant/machine!
        let run = s.orchestration.runs.last().unwrap();
        assert_eq!(run.coder.agent, "codex");
        assert!(run.coder.model.is_none());
        assert!(run.coder.machine.is_none());

        // 2. Existing session failover created a new session stub and focused it!
        assert!(s.session_by_id("codex-fallback-2").is_some());
        assert_eq!(s.selected.as_deref(), Some("codex-fallback-2"));
    }

    #[test]
    fn orchestration_preserves_original_session_title_while_allowing_manual_rename() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.hub_label = "gateway".into();
        s.sessions
            .push(test_session("orig-1", "claude", "我的原始会话标题"));
        s.selected = Some("orig-1".into());
        toggle_enabled(&mut s, Slot::Session);

        assert!(dispatch_gateway_start(
            &mut s,
            "orig-1",
            12,
            "帮我修一下样式",
            "/tmp",
            "claude",
            None,
            None,
            Some("orig-1"),
            &[],
        ));

        // Agent/backend tries to overwrite the session title during MAO rework.
        s.merge_sessions(vec![test_session(
            "orig-1",
            "claude",
            "[MAO Automated Rework · Round 2/2] 帮我修一下样式",
        )]);
        assert_eq!(
            s.session_by_id("orig-1").map(|i| i.title.as_str()),
            Some("我的原始会话标题")
        );

        // Explicit manual user rename still updates the preserved title cleanly.
        on_user_renamed_session(&mut s, "orig-1", "手动改的新标题");
        s.merge_sessions(vec![test_session("orig-1", "claude", "任何自动生成标题")]);
        assert_eq!(
            s.session_by_id("orig-1").map(|i| i.title.as_str()),
            Some("手动改的新标题")
        );
    }
}
