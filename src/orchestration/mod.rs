//! Goal-driven coordination over the existing task runtime. The coordinator
//! chooses its own work; execution observations never imply accepted quality.
mod routes;
mod store;
#[cfg(test)]
mod tests;

pub(crate) use routes::mount;
pub(crate) use store::MIGRATION;
pub(crate) const CAPABILITY: &str = "agent_orchestration_preview_v1";

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerSpec {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    client_ref: String,
    goal: String,
    cwd: String,
    coordinator: String,
    model: Option<String>,
    variant: Option<String>,
    #[serde(default)]
    asid: Option<String>,
    #[serde(default)]
    attachments: Vec<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reviewer: Option<ReviewerSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_review_rounds: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineStep {
    pub kind: String,
    pub round: u8,
    pub title: String,
    pub detail: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub time_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityGateView {
    pub id: String,
    pub status: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub severity: String,
    pub location: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunState {
    pub round: u8,
    pub max_rounds: u8,
    pub mao_phase: String,
    pub accepted: bool,
    pub coder_asid: Option<String>,
    pub reviewer_asid: Option<String>,
    pub reviewer_task_id: Option<String>,
    pub reviewed_round: u8,
    pub base_commit: Option<String>,
    pub files: Vec<ChangedFile>,
    pub coder_summary: Option<String>,
    pub review_summary: Option<String>,
    pub verdict: Option<String>,
    pub findings: Vec<ReviewFinding>,
    pub steps: Vec<PipelineStep>,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            round: 1,
            max_rounds: 2,
            mao_phase: "coding".into(),
            accepted: false,
            coder_asid: None,
            reviewer_asid: None,
            reviewer_task_id: None,
            reviewed_round: 0,
            base_commit: None,
            files: Vec::new(),
            coder_summary: None,
            review_summary: None,
            verdict: None,
            findings: Vec::new(),
            steps: Vec::new(),
        }
    }
}

pub(crate) fn git_head_sha(cwd: &str) -> Option<String> {
    let trimmed = cwd.trim();
    if trimmed.is_empty() || !std::path::Path::new(trimmed).join(".git").exists() {
        return None;
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(trimmed)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

impl RunState {
    fn for_request(request: &Start) -> Self {
        let now = crate::tasks::domain::now_ms();
        Self {
            round: 1,
            max_rounds: request.max_review_rounds.unwrap_or(2).clamp(1, 5),
            mao_phase: "coding".into(),
            accepted: false,
            coder_asid: request.asid.clone(),
            reviewer_asid: None,
            reviewer_task_id: None,
            reviewed_round: 0,
            base_commit: git_head_sha(&request.cwd),
            files: Vec::new(),
            coder_summary: None,
            review_summary: None,
            verdict: None,
            findings: Vec::new(),
            steps: vec![PipelineStep {
                kind: "coding".into(),
                round: 1,
                title: format!("Coding #1 · {}", request.coordinator),
                detail: concise_excerpt(&request.goal, 180),
                status: "running".into(),
                asid: request.asid.clone(),
                task_id: None,
                time_ms: now,
            }],
        }
    }

    pub fn quality_gates(&self) -> Vec<QualityGateView> {
        let total_add: usize = self.files.iter().map(|f| f.additions).sum();
        let total_del: usize = self.files.iter().map(|f| f.deletions).sum();

        let coding = match self.mao_phase.as_str() {
            "coding" => QualityGateView {
                id: "coding".into(),
                status: "running".into(),
                detail: format!("Round #{}", self.round),
            },
            "failed" => QualityGateView {
                id: "coding".into(),
                status: "failed".into(),
                detail: "Failed".into(),
            },
            "cancelled" => QualityGateView {
                id: "coding".into(),
                status: "idle".into(),
                detail: "Cancelled".into(),
            },
            _ => QualityGateView {
                id: "coding".into(),
                status: "completed".into(),
                detail: "Completed".into(),
            },
        };

        let diff = if !self.files.is_empty() {
            QualityGateView {
                id: "changeset".into(),
                status: "completed".into(),
                detail: format!(
                    "{} file(s) (+{} -{})",
                    self.files.len(),
                    total_add,
                    total_del
                ),
            }
        } else if matches!(self.mao_phase.as_str(), "reviewing" | "approved") {
            QualityGateView {
                id: "changeset".into(),
                status: "completed".into(),
                detail: "Captured".into(),
            }
        } else {
            QualityGateView {
                id: "changeset".into(),
                status: "idle".into(),
                detail: "Pending".into(),
            }
        };

        let review = match self.mao_phase.as_str() {
            "reviewing" => QualityGateView {
                id: "review".into(),
                status: "running".into(),
                detail: format!("Round {}/{}", self.round, self.max_rounds),
            },
            "approved" => QualityGateView {
                id: "review".into(),
                status: "completed".into(),
                detail: format!("Approved #{}", self.round),
            },
            "changes_requested" => QualityGateView {
                id: "review".into(),
                status: "warning".into(),
                detail: format!("Changes requested #{}", self.round),
            },
            "coding" if self.round > 1 => QualityGateView {
                id: "review".into(),
                status: "warning".into(),
                detail: format!("Rework #{}", self.round),
            },
            _ => QualityGateView {
                id: "review".into(),
                status: "idle".into(),
                detail: "Pending".into(),
            },
        };

        let quality = match self.mao_phase.as_str() {
            "approved" => QualityGateView {
                id: "quality".into(),
                status: "completed".into(),
                detail: "Passed".into(),
            },
            "changes_requested" => QualityGateView {
                id: "quality".into(),
                status: "warning".into(),
                detail: "Needs rework".into(),
            },
            "failed" => QualityGateView {
                id: "quality".into(),
                status: "failed".into(),
                detail: "Failed".into(),
            },
            "cancelled" => QualityGateView {
                id: "quality".into(),
                status: "idle".into(),
                detail: "Cancelled".into(),
            },
            _ => QualityGateView {
                id: "quality".into(),
                status: "idle".into(),
                detail: "Pending".into(),
            },
        };

        vec![coding, diff, review, quality]
    }
}

pub(crate) fn concise_excerpt(text: &str, max_chars: usize) -> String {
    let clean = text.trim();
    match clean.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", clean[..idx].trim_end()),
        None => clean.to_string(),
    }
}

pub(crate) fn parse_findings(reply: &str) -> Vec<ReviewFinding> {
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
            findings.push(ReviewFinding {
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

pub(crate) fn clean_review_summary(reply: &str, max_chars: usize) -> String {
    let lines: Vec<&str> = reply
        .lines()
        .filter(|line| {
            let upper = line.trim().to_ascii_uppercase();
            !upper.contains("MAO_VERDICT:") && !upper.contains("MAO_ISSUE:")
        })
        .collect();
    let cleaned = lines.join("\n");
    concise_excerpt(&cleaned, max_chars)
}

pub(crate) fn parse_verdict(reply: &str) -> (&'static str, String) {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return ("approved", String::new());
    }
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
    let excerpt = clean_review_summary(trimmed, 800);
    let findings = parse_findings(trimmed);
    let low_only = !findings.is_empty() && findings.iter().all(|f| f.severity == "low");
    match explicit {
        Some(true) => ("approved", excerpt),
        Some(false) if low_only => ("approved", excerpt),
        Some(false) => ("changes_requested", excerpt),
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
                ("changes_requested", excerpt)
            } else {
                ("approved", excerpt)
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Run {
    id: String,
    owner: String,
    request: Start,
    created_ms: u64,
    task_id: Option<String>,
    #[serde(default)]
    state: RunState,
}

impl Run {
    fn task_ref(&self) -> String {
        format!("orchestration:{}", self.id)
    }

    fn coding_prompt(&self) -> String {
        format!(
            "Implement and verify the user goal carefully. Honor repository instructions, run relevant checks/tests to confirm your changes work cleanly, and finish with changed files, commands executed, and results.\n\n\
             User goal:\n{}",
            self.request.goal
        )
    }

    fn review_prompt(
        &self,
        round: u8,
        max_rounds: u8,
        coder_summary: &str,
        diff_summary: &str,
    ) -> String {
        let diff_block = if diff_summary.trim().is_empty() {
            "(No git diff returned directly; inspect workspace files and recent changes before concluding.)"
        } else {
            diff_summary.trim()
        };
        format!(
            "You are the designated **Read-Only Code Reviewer** in an automated Coder ⇄ Reviewer pipeline (Review Round {round}/{max_rounds}).\n\n\
             STRICT RULES:\n\
             - Read-only verifier: NEVER edit, write, delete, or commit workspace source files. Only inspect diffs/files and run read-only verification checks or tests.\n\
             - NEVER ask interactive questions or wait for user confirmation; make a decisive verdict from the diff and read-only checks.\n\
             - Focus on correctness bugs, edge cases, regressions, security flaws, missing tests, or rule violations.\n\n\
             User task goal:\n{}\n\n\
             Coder summary:\n{coder_summary}\n\n\
             Changed files / Diff:\n{diff_block}\n\n\
             Output format:\n\
             - Give a concise review summary first.\n\
             - For each concrete issue found, add one line formatted as:\n\
               `MAO_ISSUE: [HIGH|MEDIUM|LOW] path:line | clear issue description and expected fix`\n\
             - Your final line MUST start with exactly one of:\n\
               - `MAO_VERDICT: APPROVED` — if clean and ready.\n\
               - `MAO_VERDICT: CHANGES_REQUESTED` — if issues must be fixed.",
            self.request.goal
        )
    }

    fn prompt(&self) -> String {
        let reviewer_section = if let Some(ref rev) = self.request.reviewer {
            let mut details = format!("agent `{}`", rev.agent);
            if let Some(ref model) = rev.model {
                details.push_str(&format!(", model `{model}`"));
            }
            if let Some(ref variant) = rev.variant {
                details.push_str(&format!(", reasoning effort `{variant}`"));
            }
            let rounds = self.request.max_review_rounds.unwrap_or(2);
            format!(
                "\nDesignated Reviewer: {details}.\n\
                 Once implementation and verification finish, delegate a code review of the changed files and diff to the designated reviewer via tasks.delegate (passing the exact agent_id, model, and variant above).\n\
                 If the reviewer requests changes, fix the issues and re-request review for up to {rounds} review round(s)."
            )
        } else {
            String::new()
        };

        format!(
            "You coordinate this software task through the Muqun task tools.\n\
             Investigate before deciding whether to delegate. Dynamically choose the work, agents,\n\
             models and dependencies from actual findings; no fixed team or task template.\n\
             Use fleet.describe to discover available agents. Delegate bounded tasks through\n\
             tasks.delegate, retaining each returned task_id. Give each worker the goal, allowed\n\
             scope, constraints, required deliverables and observable acceptance criteria.\n\
             Use tasks.read/tasks.wait to inspect actual results before scheduling dependent work.\n\
             Use exec.run for verification so exit status and output are recorded as tasks.\n\
             A worker being idle or saying done is not proof. Inspect changes and verification\n\
             evidence. Review and test the same revision; revalidate after integration.\n\
             Keep writes serial unless workers have isolated worktrees. Preserve existing changes.\n\
             Do not expand the user's permissions, answer trust prompts, publish, merge to main,\n\
             or repeat a submission whose delivery is uncertain. Honor repository instructions.\n\
             Stop repetitive failed approaches and explain the blocker instead of looping.\n\
             Finish with the changed files/revisions, verification task IDs, results, and remaining\n\
             gaps. The host presents your result for review, not as automatically accepted.{reviewer_section}\n\n\
             User goal:\n{}",
            self.request.goal
        )
    }
}
