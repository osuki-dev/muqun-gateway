//! Skills over MCP (`io.modelcontextprotocol/skills`, SEP-2640) for Muqun Gateway.
//!
//! Exposes both built-in Muqun orchestration playbooks and user-installed skills
//! from `<config_dir>/skills/<name>/SKILL.md` (with supporting subdirectories) via:
//! - native SEP-2640 `skills/list` & `skills/get` (`rmcp` typed `Skill` manifests with SHA-256 digests),
//! - standard MCP `resources/list`, `resources/read`, and `resources/directory/read`, and
//! - a compact fallback `skills.read` tool for clients that do not yet speak SEP-2640.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rmcp::model::{
    CacheScope, GetSkillResult, ListResourcesResult, ListSkillsResult, ReadResourceDirectoryResult,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, Skill, SkillFrontmatter,
    SkillResource, SkillResources, DIRECTORY_MIME_TYPE, SKILL_MD,
};
use rmcp::ErrorData;
use serde_json::{json, Value};

/// Cache freshness hint (SEP-2549) for static built-in / local skill reads (5 min).
const SKILL_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const MAX_USER_SKILLS: usize = 64;
const MAX_FILES_PER_SKILL: usize = 32;
const MAX_SKILL_FILE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct BuiltinSkill {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub body: &'static str,
}

pub(crate) const BUILTIN_SKILLS: &[BuiltinSkill] = &[
    BuiltinSkill {
        name: "fleet-and-exec",
        title: "Muqun Fleet Discovery & Command Execution",
        description: "Choose between fleet.describe, exec.run, and tasks.delegate across this computer and linked computers.",
        body: "\
---
name: fleet-and-exec
description: Choose between fleet.describe, exec.run, and tasks.delegate across this computer and linked computers.
---

# Muqun Fleet Discovery & Command Execution

Use this playbook before launching commands or sub-tasks on this computer or linked peers.

## 1. Inspect capabilities before routing
- Call `fleet.describe` (without `machine` for an overview, or with `machine` for details) to inspect reachable computers, OS, toolchains, installed agents, available models/modes, and live quota health.
- Prefer warm/healthy agents with available quota; avoid routing work to an agent whose quota health is degraded or exhausted.

## 2. Prefer `exec.run` for commands and short scripts
- When you only need to run a shell command, check status, run tests, or inspect files on a linked computer, call `exec.run(machine=...)` first — do **not** spin up a full coding agent with `tasks.delegate`.
- `exec.run` runs inside a visible terminal pane, records output, can be interrupted, and respects approvals.
- Fall back to your own SSH only if the target computer is not linked or `exec.run` cannot handle the case (interactive TUI session or directory outside home), and state why.

## 3. Delegate to an agent only when reasoning/editing is needed
- Use `tasks.delegate` when the target machine needs an autonomous coding agent to explore, edit code, iterate on failures, or synthesize findings.
",
    },
    BuiltinSkill {
        name: "parallel-tasks",
        title: "Muqun Parallel Fan-Out, Barrier Wakeup & Warm Reuse",
        description: "Run independent sub-tasks in parallel with barrier_group single-wakeup fan-in, batch tasks.wait, and reuse_task_id.",
        body: "\
---
name: parallel-tasks
description: Run independent sub-tasks in parallel with barrier_group single-wakeup fan-in, batch tasks.wait, and reuse_task_id.
---

# Muqun Parallel Fan-Out, Barrier Wakeup & Warm Reuse

Minimize turns, cold-start latency, and polling tokens when coordinating multiple sub-tasks.

## 1. Long parallel fan-out: use `barrier_group` and end your turn
- When delegating 2+ independent tasks that will take more than ~30 seconds, pass the **same** `barrier_group` string (e.g. `\"verify-pr-42\"`) on every `tasks.delegate` call, then end your turn.
- Muqun holds individual completion notices until every task in that `barrier_group` settles, then wakes you **once** with a consolidated summary of all results.
- If a child enters `waiting` (needs an answer), it surfaces immediately without deadlocking the batch.

## 2. Short parallel jobs (<= 45 s): use batch `tasks.wait`
- For quick jobs expected to finish within 45 seconds, call `tasks.wait(task_ids=[...], wait_for=\"all\" | \"any\")` once instead of polling tasks one by one.
- Do **not** repeatedly loop `tasks.wait` for multi-minute jobs; end your turn and let Muqun wake you when the barrier settles.

## 3. Reuse warm child sessions
- When iterating with a local child agent you already created (e.g. fixing review findings or running follow-up edits), pass `reuse_task_id` to `tasks.delegate` to continue that warm session instead of paying a cold start.
",
    },
    BuiltinSkill {
        name: "cross-machine-work",
        title: "Muqun Cross-Machine Delegation & File Sharing",
        description: "Coordinate work across linked computers safely: mentions, one-hop limits, unknown timeouts, and files.present.",
        body: "\
---
name: cross-machine-work
description: Coordinate work across linked computers safely: mentions, one-hop limits, unknown timeouts, and files.present.
---

# Muqun Cross-Machine Delegation & File Sharing

Follow these rules whenever a task spans multiple linked computers.

## 1. Check mentions and target machine
- Call `tasks.read()` (without `task_id`) to see if your current session is working on a Muqun task and which linked computers the user named (`mentions`).
- Pass `machine` (peer name or ID from `fleet.describe`) on `exec.run`, `tasks.delegate`, or `files.present`.

## 2. Handle uncertain network responses safely
- If `tasks.delegate(machine=...)` returns a `note` saying the peer did not answer in time, **do not delegate again**: wait on or read the returned `task_id` while the mirror reconciles.
- Cross-machine delegation is one hop from an incoming peer task (`delegation_loop`). If you cannot forward further, finish with `tasks.complete(outcome=\"blocked\", ...)` explaining what needs the other computer.

## 3. Share outputs without dumping huge payloads
- Use `files.present(path, machine=...)` to show images, logs, builds, or documents to the user in Muqun Desktop/App rather than pasting raw file dumps into chat.
",
    },
    BuiltinSkill {
        name: "task-completion",
        title: "Muqun Task Completion & Verification Protocol",
        description: "Close Muqun tasks cleanly with tasks.complete (done or blocked) and avoid redundant protocol text in delegated goals.",
        body: "\
---
name: task-completion
description: Close Muqun tasks cleanly with tasks.complete (done or blocked) and avoid redundant protocol text in delegated goals.
---

# Muqun Task Completion & Verification Protocol

## 1. Confirm whether this session owns a task
- If Muqun started your session for a task (or `tasks.read()` reports an active task), the requester sees the task as unfinished until you explicitly report back.
- If `tasks.complete` returns `task_not_found`, ignore the task protocol and do not print completion markers.

## 2. Report `done` or `blocked`
- After answering and verifying your changes, call `tasks.complete(summary=\"...\", outcome=\"done\")` with a concise one-line summary.
- When you cannot proceed without user input, credentials, or missing files, call `tasks.complete(summary=\"...\", outcome=\"blocked\")` with the exact blocker.
- Fallback only if the tool cannot be called: end your final reply with `MUQUN_TASK_DONE: <summary>` or `MUQUN_TASK_BLOCKED: <reason>`.

## 3. Keep delegated goals clean
- Do **not** repeat Muqun's completion protocol inside `tasks.delegate(goal=...)`; the child agent receives it automatically from Gateway.
",
    },
];

/// One materialized skill (built-in or loaded from `<config_dir>/skills/<name>`).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSkill {
    pub namespace: String,
    pub name: String,
    pub title: String,
    pub description: String,
    /// Relative POSIX path (e.g. `SKILL.md`, `references/guide.md`) -> UTF-8 bytes.
    pub files: BTreeMap<String, Vec<u8>>,
}

impl ResolvedSkill {
    pub(crate) fn root_uri(&self) -> String {
        format!("skill://{}/{}/", self.namespace, self.name)
    }

    pub(crate) fn uri(&self) -> String {
        format!("{}{SKILL_MD}", self.root_uri())
    }

    pub(crate) fn file_uri(&self, rel: &str) -> String {
        format!("{}{rel}", self.root_uri())
    }

    pub(crate) fn to_mcp_skill(&self) -> Skill {
        let resources: Vec<SkillResource> = self
            .files
            .iter()
            .map(|(rel, bytes)| SkillResource::from_bytes(self.file_uri(rel), bytes))
            .collect();
        Skill::new(
            self.uri(),
            SkillFrontmatter::new(self.name.clone(), self.description.clone()),
            SkillResources::Files(resources),
        )
    }

    pub(crate) fn skill_md_text(&self) -> String {
        self.files
            .get(SKILL_MD)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    }
}

fn user_skills_dir() -> Option<PathBuf> {
    crate::config_dir().ok().map(|dir| dir.join("skills"))
}

pub(crate) fn all_skills() -> Vec<ResolvedSkill> {
    all_skills_with_dir(user_skills_dir().as_deref())
}

pub(crate) fn all_skills_with_dir(user_dir: Option<&Path>) -> Vec<ResolvedSkill> {
    let mut out: Vec<ResolvedSkill> = BUILTIN_SKILLS
        .iter()
        .map(|b| {
            let mut files = BTreeMap::new();
            files.insert(SKILL_MD.to_owned(), b.body.as_bytes().to_vec());
            ResolvedSkill {
                namespace: "muqun".to_owned(),
                name: b.name.to_owned(),
                title: b.title.to_owned(),
                description: b.description.to_owned(),
                files,
            }
        })
        .collect();
    if let Some(dir) = user_dir {
        out.extend(load_user_skills(dir));
    }
    out
}

fn valid_skill_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg.len() <= 64
        && seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && !matches!(seg, "." | "..")
}

fn load_user_skills(root: &Path) -> Vec<ResolvedSkill> {
    let Ok(canonical_root) = root.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&canonical_root) else {
        return Vec::new();
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !valid_skill_segment(&name) {
                return None;
            }
            let path = e.path().canonicalize().ok()?;
            if !path.starts_with(&canonical_root) || !path.is_dir() {
                return None;
            }
            Some((name, path))
        })
        .collect();
    dirs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::new();
    for (dir_name, skill_dir) in dirs.into_iter().take(MAX_USER_SKILLS) {
        let skill_md = skill_dir.join(SKILL_MD);
        let Ok(meta) = std::fs::metadata(&skill_md) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_SKILL_FILE_BYTES {
            continue;
        }
        let Ok(body_bytes) = std::fs::read(&skill_md) else {
            continue;
        };
        let body = String::from_utf8_lossy(&body_bytes);
        let (fm_name, fm_desc) = parse_frontmatter(&body);
        // SEP-2640 requires the URI parent segment to match frontmatter `name`.
        let name = match fm_name {
            Some(n) if n == dir_name => n,
            None => dir_name.clone(),
            Some(_) => continue,
        };
        let description = fm_desc.unwrap_or_else(|| format!("User skill {name}"));
        let mut files = BTreeMap::new();
        collect_skill_files(&skill_dir, &skill_dir, "", &mut files);
        if !files.contains_key(SKILL_MD) {
            continue;
        }
        out.push(ResolvedSkill {
            namespace: "user".to_owned(),
            name: name.clone(),
            title: name,
            description,
            files,
        });
    }
    out
}

fn collect_skill_files(
    root: &Path,
    current: &Path,
    prefix: &str,
    out: &mut BTreeMap<String, Vec<u8>>,
) {
    if out.len() >= MAX_FILES_PER_SKILL {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    let mut children: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !valid_skill_segment(&name) {
                return None;
            }
            let path = e.path().canonicalize().ok()?;
            if !path.starts_with(root) {
                return None;
            }
            Some((name, path))
        })
        .collect();
    children.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path) in children {
        if out.len() >= MAX_FILES_PER_SKILL {
            break;
        }
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect_skill_files(root, &path, &rel, out);
        } else if path.is_file() {
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.len() <= MAX_SKILL_FILE_BYTES {
                    if let Ok(bytes) = std::fs::read(&path) {
                        out.insert(rel, bytes);
                    }
                }
            }
        }
    }
}

fn parse_frontmatter(body: &str) -> (Option<String>, Option<String>) {
    let mut lines = body.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("name:") {
            let val = rest.trim().trim_matches(['"', '\'']).trim();
            if !val.is_empty() {
                name = Some(val.to_owned());
            }
        } else if let Some(rest) = trimmed.strip_prefix("description:") {
            let val = rest.trim().trim_matches(['"', '\'']).trim();
            if !val.is_empty() {
                description = Some(val.to_owned());
            }
        }
    }
    (name, description)
}

fn mime_for_path(rel: &str) -> &'static str {
    if rel.ends_with(".md") {
        "text/markdown"
    } else if rel.ends_with(".json") {
        "application/json"
    } else if rel.ends_with(".yaml") || rel.ends_with(".yml") {
        "application/yaml"
    } else {
        "text/plain"
    }
}

pub(crate) fn find_skill(key: &str) -> Option<ResolvedSkill> {
    find_skill_in(key, &all_skills())
}

fn find_skill_in(key: &str, skills: &[ResolvedSkill]) -> Option<ResolvedSkill> {
    let trimmed = key.trim();
    skills
        .iter()
        .find(|s| {
            s.name == trimmed
                || s.uri() == trimmed
                || format!("{}/{}", s.namespace, s.name) == trimmed
        })
        .cloned()
}

pub(crate) fn list_resources() -> ListResourcesResult {
    list_resources_in(&all_skills())
}

fn list_resources_in(skills: &[ResolvedSkill]) -> ListResourcesResult {
    let resources = skills
        .iter()
        .map(|skill| {
            let size = skill
                .files
                .get(SKILL_MD)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            Resource::new(skill.uri(), skill.name.clone())
                .with_title(skill.title.clone())
                .with_description(skill.description.clone())
                .with_mime_type("text/markdown")
                .with_size(size)
        })
        .collect();
    ListResourcesResult::with_all_items(resources)
}

pub(crate) fn read_resource(uri: &str) -> Result<ReadResourceResponse, ErrorData> {
    read_resource_in(uri, &all_skills())
}

fn read_resource_in(
    uri: &str,
    skills: &[ResolvedSkill],
) -> Result<ReadResourceResponse, ErrorData> {
    for skill in skills {
        let root = skill.root_uri();
        if let Some(rel) = uri.strip_prefix(&root) {
            if let Some(bytes) = skill.files.get(rel) {
                let text = String::from_utf8_lossy(bytes).into_owned();
                return Ok(ReadResourceResponse::Complete(
                    ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
                        uri: uri.to_owned(),
                        mime_type: Some(mime_for_path(rel).to_owned()),
                        text,
                        meta: None,
                    }])
                    .with_ttl_ms(SKILL_CACHE_TTL_MS)
                    .with_cache_scope(CacheScope::Public),
                ));
            }
        }
    }
    Err(ErrorData::resource_not_found(
        format!("unknown skill resource: {uri}"),
        None,
    ))
}

/// Official SEP-2640 `resources/directory/read` for browsing files/subdirectories
/// inside a skill URI (e.g. `skill://user/my-skill/` or `skill://user/my-skill/references/`).
pub(crate) fn read_resource_directory(uri: &str) -> Result<ReadResourceDirectoryResult, ErrorData> {
    read_resource_directory_in(uri, &all_skills())
}

fn read_resource_directory_in(
    uri: &str,
    skills: &[ResolvedSkill],
) -> Result<ReadResourceDirectoryResult, ErrorData> {
    let dir_uri = if uri.ends_with('/') {
        uri.to_owned()
    } else {
        format!("{uri}/")
    };
    for skill in skills {
        let root = skill.root_uri();
        let Some(sub_prefix) = dir_uri.strip_prefix(&root) else {
            continue;
        };
        let mut subdirs = BTreeMap::<String, String>::new();
        let mut files = Vec::new();
        for (rel, bytes) in &skill.files {
            let Some(rest) = rel.strip_prefix(sub_prefix) else {
                continue;
            };
            if let Some((head, _tail)) = rest.split_once('/') {
                subdirs
                    .entry(head.to_owned())
                    .or_insert_with(|| format!("{dir_uri}{head}/"));
            } else if !rest.is_empty() {
                files.push(
                    Resource::new(format!("{dir_uri}{rest}"), rest)
                        .with_mime_type(mime_for_path(rel))
                        .with_size(bytes.len() as u64),
                );
            }
        }
        if subdirs.is_empty() && files.is_empty() {
            return Err(ErrorData::resource_not_found(
                format!("unknown skill directory: {uri}"),
                None,
            ));
        }
        let mut entries: Vec<Resource> = subdirs
            .into_iter()
            .map(|(name, child_uri)| {
                Resource::new(child_uri, name).with_mime_type(DIRECTORY_MIME_TYPE)
            })
            .collect();
        entries.extend(files);
        return Ok(ReadResourceDirectoryResult::with_all_items(entries));
    }
    Err(ErrorData::resource_not_found(
        format!("unknown skill directory: {uri}"),
        None,
    ))
}

/// Official SEP-2640 `skills/list` response.
pub(crate) fn list_skills() -> ListSkillsResult {
    list_skills_in(&all_skills())
}

fn list_skills_in(skills: &[ResolvedSkill]) -> ListSkillsResult {
    let items = skills.iter().map(ResolvedSkill::to_mcp_skill).collect();
    ListSkillsResult::with_all_items(items)
}

/// Official SEP-2640 `skills/get` response.
pub(crate) fn get_skill(uri: &str) -> Result<GetSkillResult, ErrorData> {
    get_skill_in(uri, &all_skills())
}

fn get_skill_in(uri: &str, skills: &[ResolvedSkill]) -> Result<GetSkillResult, ErrorData> {
    let Some(skill) = skills.iter().find(|s| s.uri() == uri) else {
        return Err(ErrorData::resource_not_found(
            format!("unknown skill: {uri}"),
            None,
        ));
    };
    Ok(GetSkillResult::new(skill.to_mcp_skill())
        .with_ttl_ms(SKILL_CACHE_TTL_MS)
        .with_cache_scope(CacheScope::Public))
}

/// Compact catalog returned when `skills.read` is called without a name.
pub(crate) fn skills_catalog_json() -> Value {
    let items: Vec<Value> = all_skills()
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "namespace": s.namespace,
                "uri": s.uri(),
                "description": s.description,
                "file_count": s.files.len(),
            })
        })
        .collect();
    json!({ "skills": items })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_user_skills_with_subdirectories_and_directory_read() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("deploy-helper");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy-helper\ndescription: Release checklist\n---\n# Deploy\n",
        )
        .unwrap();
        std::fs::write(
            skill_dir.join("references/checks.md"),
            "# Checks\nRun cargo test\n",
        )
        .unwrap();

        let skills = all_skills_with_dir(Some(dir.path()));
        let listed = list_skills_in(&skills);
        let found = listed
            .skills
            .iter()
            .find(|s| s.uri == "skill://user/deploy-helper/SKILL.md")
            .expect("user skill listed");
        found.validate().expect("SEP-2640 valid");

        // Directory browsing works for both root and nested folders.
        let root_dir = read_resource_directory_in("skill://user/deploy-helper/", &skills).unwrap();
        assert!(root_dir
            .resources
            .iter()
            .any(|r| r.name == "references" && r.is_directory()));
        assert!(root_dir.resources.iter().any(|r| r.name == "SKILL.md"));

        let refs_dir =
            read_resource_directory_in("skill://user/deploy-helper/references/", &skills).unwrap();
        assert_eq!(refs_dir.resources.len(), 1);
        assert_eq!(refs_dir.resources[0].name, "checks.md");

        let read =
            read_resource_in("skill://user/deploy-helper/references/checks.md", &skills).unwrap();
        let ReadResourceResponse::Complete(res) = read else {
            panic!("expected complete read");
        };
        assert_eq!(res.ttl_ms, Some(SKILL_CACHE_TTL_MS));
        assert_eq!(res.cache_scope, Some(CacheScope::Public));
    }
}
