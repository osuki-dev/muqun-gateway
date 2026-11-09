//! Which of an ACP agent's slash commands are skills.
//!
//! Claude Code (through `claude-agent-acp`), Cursor and Qoder list every
//! skill they load as a slash command (`/brainstorming`,
//! `/superpowers:brainstorming`) beside their built-in ones, with nothing in
//! `available_commands_update` to say which entries are skills: the Claude
//! adapter marks them (`_meta.jetbrains.air.kind = "skill"`) for JetBrains
//! AIR clients only, and declaring the Gateway one changes much else about
//! the session. So the split is made the way the adapter itself makes it:
//! by the skill files on disk. A listed command whose name a `SKILL.md`
//! declares in one of the agent's skill directories is offered as a skill
//! (`catalog.skills`, for the App's `$` picker) as well as left as the
//! command that runs it (`/name args` keeps working, arguments included).
//!
//! A skill the CLI bundles without a file on disk (`claude-api`, `dataviz`,
//! …) cannot be told from a built-in command this way and stays a command
//! only.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::agents::domain::{CommandInfo, SkillInfo};

/// Where one agent family reads its skills from.
pub struct SkillLayout {
    /// Directories holding `<name>/SKILL.md`, relative to the workspace
    /// (each level up to the checkout root) and to the home directory.
    pub containers: &'static [&'static str],
    /// Reads Claude Code's own config directory as well: `$CLAUDE_CONFIG_DIR`
    /// (default `~/.claude`) holds `skills/`, `commands/` (prompt commands,
    /// which are never skills even when a skill elsewhere declares the same
    /// name) and `plugins/installed_plugins.json`, whose plugins' skills the
    /// agent lists as `<plugin>:<name>`.
    pub claude_config: bool,
}

/// Claude Code: `.claude/skills` plus the cross-agent `.agents/skills` the
/// adapter probes, its config directory and its installed plugins.
pub const CLAUDE: SkillLayout = SkillLayout {
    containers: &[".claude/skills", ".agents/skills"],
    claude_config: true,
};

/// Cursor CLI (`src/utils/skill-path-utils.ts` of the 2026.10 build): its
/// own directories and every other agent's it reads.
pub const CURSOR: SkillLayout = SkillLayout {
    containers: &[
        ".cursor/skills",
        ".cursor/skills-cursor",
        ".claude/skills",
        ".codex/skills",
        ".agents/skills",
    ],
    claude_config: false,
};

/// Qoder CLI: `~/.qoder/skills` and a repo's `.qoder/skills`.
pub const QODER: SkillLayout = SkillLayout {
    containers: &[".qoder/skills", ".agents/skills"],
    claude_config: false,
};

/// How many manifests one scan may read, duplicates included: the same
/// skill is commonly linked into several agents' directories.
const MAX_MANIFEST_READS: usize = 2_000;
/// Entries read from one skills directory.
const MAX_ENTRIES_PER_DIR: usize = 512;
/// Front matter is at the head of the file, so a larger `SKILL.md` is not
/// read at all; a link to `/dev/zero` must not turn a scan into a hang.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Plugins read from `installed_plugins.json`.
const MAX_PLUGINS: usize = 256;

/// The names of every skill `layout` finds for a session in `cwd`, spelled
/// as the agent lists them (`name`, `plugin:name`). Blocking filesystem
/// work: call it off the async runtime.
pub(crate) fn skill_names(
    layout: &SkillLayout,
    cwd: &Path,
    home: Option<&Path>,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> HashSet<String> {
    let mut bases = crate::composer::workspace_levels(cwd, home);
    bases.extend(home.map(Path::to_path_buf));
    let mut budget = MAX_MANIFEST_READS;
    let mut names = HashSet::new();
    for base in &bases {
        for container in layout.containers {
            collect(&base.join(container), "", &mut budget, &mut names);
        }
    }
    if !layout.claude_config {
        return names;
    }
    let config = match env("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => match home {
            Some(home) => home.join(".claude"),
            None => return names,
        },
    };
    collect(&config.join("skills"), "", &mut budget, &mut names);
    for (plugin, install) in installed_plugins(&config.join("plugins/installed_plugins.json")) {
        collect(
            &install.join("skills"),
            &format!("{plugin}:"),
            &mut budget,
            &mut names,
        );
    }
    // A prompt command of `.claude/commands` is no skill, whatever a skill
    // directory elsewhere declares under its name.
    for dir in bases
        .iter()
        .map(|base| base.join(".claude/commands"))
        .chain([config.join("commands")])
    {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten().take(MAX_ENTRIES_PER_DIR) {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("md") {
                if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                    names.remove(stem);
                }
            }
        }
    }
    names
}

/// Every `<dir>/<entry>/SKILL.md`, by the name its front matter declares,
/// else its directory's. Claude Code advertises a skill by the declared
/// name, which can differ from the directory holding it.
fn collect(dir: &Path, prefix: &str, budget: &mut usize, out: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().take(MAX_ENTRIES_PER_DIR) {
        if *budget == 0 {
            return;
        }
        let fallback = entry.file_name();
        let Some(fallback) = fallback.to_str().filter(|name| skill_name(name)) else {
            continue;
        };
        let manifest = entry.path().join("SKILL.md");
        let Ok(meta) = std::fs::metadata(&manifest) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_MANIFEST_BYTES {
            continue;
        }
        *budget -= 1;
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let name = crate::composer::field(&text, "name")
            .filter(|name| skill_name(name))
            .unwrap_or_else(|| fallback.to_owned());
        out.insert(format!("{prefix}{name}"));
    }
}

/// A name the agent could list as a command: one token, no path separators.
fn skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != ".."
        && !name
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '\\'))
}

/// `(plugin, installPath)` of every plugin Claude Code has installed:
/// `{"plugins": {"<plugin>@<marketplace>": [{"installPath": "…"}]}}`.
fn installed_plugins(manifest: &Path) -> Vec<(String, PathBuf)> {
    let Ok(meta) = std::fs::metadata(manifest) else {
        return Vec::new();
    };
    if !meta.is_file() || meta.len() > 4 * 1024 * 1024 {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(plugins) = value["plugins"].as_object() else {
        return Vec::new();
    };
    plugins
        .iter()
        .take(MAX_PLUGINS)
        .filter_map(|(key, installs)| {
            let plugin = key.split('@').next().filter(|plugin| skill_name(plugin))?;
            // Several installs of one plugin (user and project scope) share
            // its name; the first listed is the one Claude Code loads first.
            let install = installs
                .as_array()?
                .iter()
                .find_map(|install| install["installPath"].as_str())?;
            Some((plugin.to_owned(), PathBuf::from(install)))
        })
        .collect()
}

/// The commands that are skills, as the App's `$` picker lists them: the
/// command's name is the skill's id, its description the skill's. `slash`
/// because `/name` is exactly how the agent runs one.
pub(crate) fn split(commands: &[CommandInfo], names: &HashSet<String>) -> Vec<SkillInfo> {
    commands
        .iter()
        .filter(|command| names.contains(&command.name))
        .map(|command| SkillInfo {
            id: command.name.clone(),
            name: command.name.clone(),
            description: command.description.clone().unwrap_or_default(),
            slash: true,
            autoinvoke: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn skill(root: &Path, dir: &str, name: &str, front: &str) {
        write(
            &root.join(dir).join(name).join("SKILL.md"),
            &format!("---\n{front}description: d\n---\nbody\n"),
        );
    }

    /// A scratch directory removed when dropped.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One fixture home and checkout, read by every layout.
    fn fixture() -> (Scratch, PathBuf, PathBuf) {
        let temp = Scratch(
            std::env::temp_dir().join(format!("muqun-acp-skills-{}", uuid::Uuid::new_v4())),
        );
        let home = temp.0.join("home");
        let repo = home.join("src/repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // User skills: one named by its directory, one by its front matter.
        skill(&home, ".claude/skills", "alpha", "");
        skill(&home, ".claude/skills", "dir-name", "name: declared\n");
        // A skill whose name a prompt command also carries: the command wins.
        skill(&home, ".claude/skills", "gamma", "");
        write(
            &home.join(".claude/commands/gamma.md"),
            "---\ndescription: x\n---\n",
        );
        // Cross-agent and other agents' directories.
        skill(&home, ".agents/skills", "shared", "");
        skill(&home, ".cursor/skills", "cur", "");
        skill(&home, ".qoder/skills", "qod", "");
        // An installed plugin, found through the manifest, not by probing.
        let install = home.join(".claude/plugins/cache/mp/power/1.0.0");
        skill(&install, "skills", "brainstorm", "");
        write(
            &home.join(".claude/plugins/installed_plugins.json"),
            &serde_json::json!({"version": 2, "plugins": {
                "power@mp": [{"installPath": install, "scope": "user"}]
            }})
            .to_string(),
        );
        // Project skills at the checkout root, read for a session below it.
        skill(&repo, ".claude/skills", "proj", "");
        skill(&repo, ".agents/skills", "proj-shared", "");
        // Not skills: a directory without a manifest, a manifest that is not
        // a file, and a directory the scan never looks in.
        std::fs::create_dir_all(home.join(".claude/skills/empty")).unwrap();
        std::fs::create_dir_all(home.join(".claude/skills/odd/SKILL.md")).unwrap();
        skill(&home, "elsewhere", "hidden", "");
        let cwd = repo.join("deep");
        std::fs::create_dir_all(&cwd).unwrap();
        (temp, home, cwd)
    }

    #[test]
    fn each_layout_finds_its_own_skill_files_and_nothing_else() {
        let (_temp, home, cwd) = fixture();
        for (label, layout, want) in [
            (
                "claude",
                &CLAUDE,
                &[
                    "alpha",
                    "declared",
                    "shared",
                    "power:brainstorm",
                    "proj",
                    "proj-shared",
                ][..],
            ),
            (
                "cursor",
                &CURSOR,
                &[
                    "alpha",
                    "declared",
                    "gamma",
                    "shared",
                    "cur",
                    "proj",
                    "proj-shared",
                ][..],
            ),
            ("qoder", &QODER, &["shared", "qod", "proj-shared"][..]),
        ] {
            let found = skill_names(layout, &cwd, Some(&home), |_| None);
            let mut found: Vec<_> = found.into_iter().collect();
            found.sort();
            let mut want: Vec<_> = want.iter().map(|name| name.to_string()).collect();
            want.sort();
            assert_eq!(found, want, "{label}");
        }
    }

    #[test]
    fn claude_config_dir_replaces_the_default_and_the_split_keeps_commands() {
        let (temp, home, cwd) = fixture();
        let config = temp.0.join("cfg");
        skill(&config, "skills", "cfg-skill", "");
        let found = skill_names(&CLAUDE, &cwd, Some(&home), |key| {
            (key == "CLAUDE_CONFIG_DIR").then(|| config.clone().into_os_string())
        });
        assert!(found.contains("cfg-skill"));
        // `~/.claude/skills` is still a container of its own; the config
        // directory's plugins are the ones read, so `power:` is gone.
        assert!(found.contains("alpha") && !found.contains("power:brainstorm"));

        let command = |name: &str, description: Option<&str>| CommandInfo {
            name: name.into(),
            description: description.map(str::to_owned),
            mode: None,
            template: None,
        };
        let commands = vec![
            command("compact", Some("Summarize")),
            command("alpha", Some("Alpha skill")),
            command("power:brainstorm", None),
        ];
        let skills = split(&commands, &found);
        assert_eq!(
            skills
                .iter()
                .map(|skill| (skill.id.as_str(), skill.description.as_str(), skill.slash))
                .collect::<Vec<_>>(),
            vec![("alpha", "Alpha skill", true)]
        );
        assert_eq!(commands.len(), 3, "the commands list is left whole");
    }
}
