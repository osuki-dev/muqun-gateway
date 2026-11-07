//! Projects (architecture §6.1b′): the folders sessions run in, derived --
//! never created by hand.
//!
//! A project is a directory: the git root of a session's folder when it is
//! in a checkout, else the folder itself. They come from every listed
//! session's `directory`, every agent's `list_projects()` and the folders
//! remembered per agent (`general.last_cwd`). The owner can pin one, hide
//! one or give it another name ([`ProjectPrefs`], kept in the config); a
//! folder that has gone is greyed, never removed.
//!
//! [`resolve`] touches the filesystem and runs on the core side, off the UI
//! thread; [`derive`] and [`flatten`] are pure and run when the sidebar lays
//! itself out.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use muqun_gateway::agents::AgentProject;
use serde::{Deserialize, Serialize};

use super::types::ListedSession;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    /// The root of a git checkout (or worktree).
    Git,
    /// A folder outside any checkout.
    Plain,
}

/// What one session folder turned out to be on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirInfo {
    /// The project the folder belongs to: its checkout's root, or itself.
    pub root: PathBuf,
    pub kind: ProjectKind,
    /// The checkout's current branch (a short hash when detached).
    pub branch: Option<String>,
    /// The folder exists on this computer.
    pub exists: bool,
}

/// Where `dir` belongs, as `git rev-parse --show-toplevel` would say, read
/// straight off the filesystem (the gateway's git helper is not public, and
/// spawning git for 300 folders is slower than looking for `.git`). Blocking.
pub fn resolve(dir: &Path) -> DirInfo {
    if !dir.is_dir() {
        return DirInfo {
            root: dir.to_path_buf(),
            kind: ProjectKind::Plain,
            branch: None,
            exists: false,
        };
    }
    for candidate in dir.ancestors() {
        let git = candidate.join(".git");
        if git.exists() {
            return DirInfo {
                root: candidate.to_path_buf(),
                kind: ProjectKind::Git,
                branch: branch(&git),
                exists: true,
            };
        }
    }
    DirInfo {
        root: dir.to_path_buf(),
        kind: ProjectKind::Plain,
        branch: None,
        exists: true,
    }
}

/// The branch `HEAD` names, for a `.git` directory or a worktree's `.git`
/// file (`gitdir: <path>`).
fn branch(git: &Path) -> Option<String> {
    let dir = if git.is_file() {
        let text = std::fs::read_to_string(git).ok()?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        let target = Path::new(target);
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            git.parent()?.join(target)
        }
    } else {
        git.to_path_buf()
    };
    head_branch(&std::fs::read_to_string(dir.join("HEAD")).ok()?)
}

/// `ref: refs/heads/main` → `main`; a detached hash → its first 7 digits.
fn head_branch(head: &str) -> Option<String> {
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(reference) => {
            let reference = reference.trim();
            Some(
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(reference)
                    .to_string(),
            )
        }
        None if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) => {
            Some(head[..7].to_string())
        }
        None => None,
    }
}

/// What the owner set for one project, by root, in the config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectPrefs {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Shown instead of the folder name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ProjectPrefs {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The root as a string; empty for the sessions that name no folder.
    pub id: String,
    pub root: PathBuf,
    /// (muted parent with its slash, bold name): `muqun/` + `gateway`.
    pub display: (String, String),
    pub kind: ProjectKind,
    pub pinned: bool,
    pub hidden: bool,
    /// The folder is not on this computer any more.
    pub missing: bool,
    /// How recently it was active: the place of its newest session in the
    /// newest-first session list (`usize::MAX` with no sessions). A rank,
    /// not a time, so a busy session touching its timestamp moves nothing.
    pub last_active: usize,
    pub branch: Option<String>,
    /// Its sessions, newest first.
    pub sessions: Vec<String>,
}

impl Project {
    /// The id of the group of sessions without a folder.
    pub const NO_FOLDER: &'static str = "";
    /// Scratch / overflow projects folded into one collapsed group.
    pub const OTHER: &'static str = "__other__";
    /// How many ordinary projects the sidebar shows before the rest join
    /// [`Self::OTHER`] (pinned ones always keep a seat).
    pub const MAX_PRIMARY: usize = 8;

    pub fn is_no_folder(&self) -> bool {
        self.id.is_empty()
    }

    pub fn is_other(&self) -> bool {
        self.id == Self::OTHER
    }
}

/// Temp, cache and missing folders: not worth a project row of their own.
pub fn is_scratch(root: &Path, missing: bool, home: Option<&Path>) -> bool {
    if missing {
        return true;
    }
    if root.starts_with("/tmp") || root.starts_with("/var/tmp") {
        return true;
    }
    if let Ok(cache) = std::env::var("XDG_CACHE_HOME") {
        if !cache.is_empty() && root.starts_with(Path::new(&cache)) {
            return true;
        }
    }
    home.is_some_and(|home| root.starts_with(home.join(".cache")))
}

/// Everything a project list is derived from.
pub struct Sources<'a> {
    /// Every listed session, newest first.
    pub sessions: &'a [ListedSession],
    /// What each session folder resolved to, by folder.
    pub dirs: &'a HashMap<String, DirInfo>,
    /// The projects the agents know.
    pub known: &'a [AgentProject],
    /// Folders sessions were last started in.
    pub recent: &'a [PathBuf],
    pub prefs: &'a BTreeMap<String, ProjectPrefs>,
    /// Start-page session pins (`general.pinned`): a project that holds one
    /// stays out of Other, same as an explicit project pin.
    pub session_pins: &'a [String],
    pub home: Option<&'a Path>,
}

/// `dir` without a trailing separator (but `/` stays `/`).
fn normalize(dir: &str) -> &str {
    let trimmed = dir.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        &dir[..dir.len().min(1)]
    } else {
        trimmed
    }
}

/// (muted parent, name) for `root`: the parent is left out when it is the
/// home folder or the filesystem root.
fn display(root: &Path, home: Option<&Path>, name: Option<&str>) -> (String, String) {
    if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
        return (String::new(), name.trim().to_string());
    }
    let Some(base) = root.file_name() else {
        return (String::new(), root.to_string_lossy().into_owned());
    };
    let parent = root
        .parent()
        .filter(|p| p.parent().is_some() && Some(*p) != home)
        .and_then(|p| p.file_name())
        .map(|p| format!("{}/", p.to_string_lossy()))
        .unwrap_or_default();
    (parent, base.to_string_lossy().into_owned())
}

/// Every project, pinned first, then by last activity; the folder-less
/// sessions last. Agent-known projects whose folder is gone and that have no
/// sessions here are left out: there is nothing to show under them.
pub fn derive(src: &Sources) -> Vec<Project> {
    let mut found = Found {
        src,
        projects: Vec::new(),
        by_root: HashMap::new(),
    };
    let mut loose: Vec<String> = Vec::new();
    for (rank, session) in src.sessions.iter().enumerate() {
        match session
            .directory
            .as_deref()
            .and_then(|dir| found.add(dir, false, rank))
        {
            Some(at) => found.projects[at].sessions.push(session.asid.clone()),
            None => loose.push(session.asid.clone()),
        }
    }
    for known in src.known {
        if known.missing && !found.has(&known.canonical) {
            continue;
        }
        found.add(&known.canonical, known.missing, usize::MAX);
    }
    for dir in src.recent {
        found.add(&dir.to_string_lossy(), false, usize::MAX);
    }
    let mut projects = found.projects;
    // Session pins and project prefs share the same seat: either keeps the
    // project in the primary list.
    if !src.session_pins.is_empty() {
        for project in &mut projects {
            if !project.pinned
                && project
                    .sessions
                    .iter()
                    .any(|s| src.session_pins.iter().any(|p| p == s))
            {
                project.pinned = true;
            }
        }
    }
    // Stable: equal ranks keep the order they were found in.
    projects.sort_by_key(|p| (!p.pinned, p.last_active));
    if !loose.is_empty() {
        projects.push(Project {
            id: Project::NO_FOLDER.to_string(),
            root: PathBuf::new(),
            display: (String::new(), String::new()),
            kind: ProjectKind::Plain,
            pinned: false,
            hidden: false,
            missing: false,
            last_active: usize::MAX,
            branch: None,
            sessions: loose,
        });
    }
    fold_other(projects, src.home)
}

/// Scratch folders and projects past [`Project::MAX_PRIMARY`] share one
/// collapsed "Other (N)" row; pinned projects always keep a seat.
fn fold_other(projects: Vec<Project>, home: Option<&Path>) -> Vec<Project> {
    let mut primary: Vec<Project> = Vec::new();
    let mut other_sessions: Vec<String> = Vec::new();
    let mut other_active = usize::MAX;
    let mut seats = 0usize;
    for project in projects {
        if project.is_no_folder() {
            primary.push(project);
            continue;
        }
        let scratch = is_scratch(&project.root, project.missing, home);
        let take = project.pinned || (!scratch && seats < Project::MAX_PRIMARY);
        if take {
            seats += 1;
            primary.push(project);
        } else {
            other_active = other_active.min(project.last_active);
            other_sessions.extend(project.sessions);
        }
    }
    if !other_sessions.is_empty() {
        let n = other_sessions.len();
        let other = Project {
            id: Project::OTHER.to_string(),
            root: PathBuf::new(),
            // UI replaces this with a localised "Other (N)".
            display: (String::new(), format!("Other ({n})")),
            kind: ProjectKind::Plain,
            pinned: false,
            hidden: false,
            missing: false,
            last_active: other_active,
            branch: None,
            sessions: other_sessions,
        };
        // Keep the folder-less group last, after Other.
        match primary.iter().position(|p| p.is_no_folder()) {
            Some(at) => primary.insert(at, other),
            None => primary.push(other),
        }
    }
    primary
}

/// The projects found so far, by root.
struct Found<'a, 'b> {
    src: &'b Sources<'a>,
    projects: Vec<Project>,
    by_root: HashMap<String, usize>,
}

impl Found<'_, '_> {
    /// The root `dir` belongs to, as a project id.
    fn root_of(&self, dir: &str) -> (String, Option<&DirInfo>) {
        let info = self.src.dirs.get(dir);
        let root = info.map_or_else(
            || dir.to_string(),
            |i| i.root.to_string_lossy().into_owned(),
        );
        (root, info)
    }

    fn has(&self, dir: &str) -> bool {
        self.by_root.contains_key(&self.root_of(normalize(dir)).0)
    }

    /// The project of `dir`, added when new; `rank` lowers its last
    /// activity. `None` for an empty folder name.
    fn add(&mut self, dir: &str, missing_hint: bool, rank: usize) -> Option<usize> {
        let dir = normalize(dir);
        if dir.is_empty() {
            return None;
        }
        let (id, info) = self.root_of(dir);
        if let Some(&at) = self.by_root.get(&id) {
            let project = &mut self.projects[at];
            project.last_active = project.last_active.min(rank);
            return Some(at);
        }
        let info = info.cloned();
        let root = PathBuf::from(&id);
        let prefs = self.src.prefs.get(&id).cloned().unwrap_or_default();
        self.projects.push(Project {
            display: display(&root, self.src.home, prefs.name.as_deref()),
            id: id.clone(),
            root,
            kind: info.as_ref().map_or(ProjectKind::Plain, |i| i.kind),
            pinned: prefs.pinned,
            hidden: prefs.hidden,
            missing: missing_hint || info.as_ref().is_some_and(|i| !i.exists),
            last_active: rank,
            branch: info.and_then(|i| i.branch),
            sessions: Vec::new(),
        });
        self.by_root.insert(id, self.projects.len() - 1);
        Some(self.projects.len() - 1)
    }
}

/// One line of the sidebar tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    /// Space above every project but the first.
    Gap,
    Project {
        id: String,
        open: bool,
    },
    Session {
        asid: String,
        project: String,
    },
    /// "More (N)" under a project cut at [`TreeOptions::per_project`].
    More {
        project: String,
        hidden: usize,
    },
}

pub struct TreeOptions<'a> {
    /// Lowercased search text; empty shows everything.
    pub query: &'a str,
    /// Projects the owner opened or closed, by id.
    pub open: &'a HashMap<String, bool>,
    /// Projects the owner opened all the way ("More").
    pub full: &'a std::collections::HashSet<String>,
    /// How many of the first projects start open.
    pub default_open: usize,
    /// Sessions a project shows before "More".
    pub per_project: usize,
}

/// The tree as rows, top to bottom. Hidden projects are left out unless a
/// search names them. While searching, every project with a hit is open and
/// shows all of its hits.
pub fn flatten(
    projects: &[Project],
    options: &TreeOptions,
    title: impl Fn(&str) -> Option<String>,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let query = options.query;
    let mut shown = 0usize;
    for project in projects {
        let (sessions, open): (Vec<&String>, bool) = if query.is_empty() {
            if project.hidden {
                continue;
            }
            let open = options
                .open
                .get(&project.id)
                .copied()
                .unwrap_or(shown < options.default_open);
            (project.sessions.iter().collect(), open)
        } else {
            let named = project.display.1.to_lowercase().contains(query)
                || project.id.to_lowercase().contains(query);
            let hits: Vec<&String> = project
                .sessions
                .iter()
                .filter(|asid| title(asid).is_some_and(|t| t.to_lowercase().contains(query)))
                .collect();
            if named {
                (project.sessions.iter().collect(), true)
            } else if hits.is_empty() {
                continue;
            } else {
                (hits, true)
            }
        };
        if shown > 0 {
            rows.push(TreeRow::Gap);
        }
        shown += 1;
        rows.push(TreeRow::Project {
            id: project.id.clone(),
            open,
        });
        if !open {
            continue;
        }
        let limit = if !query.is_empty() || options.full.contains(&project.id) {
            usize::MAX
        } else {
            options.per_project
        };
        for asid in sessions.iter().take(limit) {
            rows.push(TreeRow::Session {
                asid: (*asid).clone(),
                project: project.id.clone(),
            });
        }
        if sessions.len() > limit {
            rows.push(TreeRow::More {
                project: project.id.clone(),
                hidden: sessions.len() - limit,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn listed(asid: &str, dir: Option<&str>) -> ListedSession {
        ListedSession {
            asid: asid.into(),
            directory: dir.map(Into::into),
        }
    }

    fn git(root: &str, branch: &str) -> DirInfo {
        DirInfo {
            root: root.into(),
            kind: ProjectKind::Git,
            branch: Some(branch.into()),
            exists: true,
        }
    }

    fn known(dir: &str, missing: bool) -> AgentProject {
        AgentProject {
            id: dir.into(),
            canonical: dir.into(),
            name: dir.into(),
            vcs: None,
            sandboxes: vec![],
            missing,
        }
    }

    struct Fixture {
        sessions: Vec<ListedSession>,
        dirs: HashMap<String, DirInfo>,
        known: Vec<AgentProject>,
        recent: Vec<PathBuf>,
        prefs: BTreeMap<String, ProjectPrefs>,
        session_pins: Vec<String>,
    }

    impl Fixture {
        fn derive(&self) -> Vec<Project> {
            derive(&Sources {
                sessions: &self.sessions,
                dirs: &self.dirs,
                known: &self.known,
                recent: &self.recent,
                prefs: &self.prefs,
                session_pins: &self.session_pins,
                home: Some(Path::new("/home/u")),
            })
        }
    }

    fn fixture() -> Fixture {
        let mut dirs = HashMap::new();
        dirs.insert(
            "/home/u/w/muqun/gateway".into(),
            git("/home/u/w/muqun/gateway", "main"),
        );
        dirs.insert(
            "/home/u/w/muqun/gateway/desktop".into(),
            git("/home/u/w/muqun/gateway", "main"),
        );
        dirs.insert(
            "/home/u/w/muqun/app".into(),
            git("/home/u/w/muqun/app", "dev"),
        );
        Fixture {
            sessions: vec![
                listed("a", Some("/home/u/w/muqun/gateway/desktop")),
                listed("b", Some("/home/u/w/muqun/app/")),
                listed("c", None),
                listed("d", Some("/home/u/w/muqun/gateway")),
                listed("e", Some("/home/u/dotfiles")),
            ],
            dirs,
            known: vec![],
            recent: vec![],
            prefs: BTreeMap::new(),
            session_pins: vec![],
        }
    }

    fn ids(projects: &[Project]) -> Vec<&str> {
        projects.iter().map(|p| p.id.as_str()).collect()
    }

    #[test]
    fn sessions_group_under_their_checkout_root_most_recent_first() {
        let projects = fixture().derive();
        assert_eq!(
            ids(&projects),
            [
                "/home/u/w/muqun/gateway",
                "/home/u/w/muqun/app",
                "/home/u/dotfiles",
                ""
            ]
        );
        assert_eq!(
            projects[0].sessions,
            ["a", "d"],
            "a subfolder joins its repo"
        );
        assert_eq!(projects[0].kind, ProjectKind::Git);
        assert_eq!(projects[0].branch.as_deref(), Some("main"));
        assert_eq!(
            projects[0].display,
            ("muqun/".to_string(), "gateway".to_string())
        );
        assert_eq!(
            projects[1].sessions,
            ["b"],
            "a trailing slash is the same folder"
        );
        assert_eq!(
            projects[2].display,
            (String::new(), "dotfiles".to_string()),
            "no parent shown for a folder in home"
        );
        assert_eq!(projects[2].kind, ProjectKind::Plain, "unresolved yet");
        assert!(projects[3].is_no_folder());
        assert_eq!(projects[3].sessions, ["c"]);
    }

    #[test]
    fn pinned_projects_come_first_and_names_can_be_overridden() {
        let mut f = fixture();
        f.prefs.insert(
            "/home/u/dotfiles".into(),
            ProjectPrefs {
                pinned: true,
                name: Some("Dots".into()),
                ..ProjectPrefs::default()
            },
        );
        let projects = f.derive();
        assert_eq!(projects[0].id, "/home/u/dotfiles");
        assert!(projects[0].pinned);
        assert_eq!(projects[0].display, (String::new(), "Dots".to_string()));
        assert_eq!(projects[1].id, "/home/u/w/muqun/gateway");
    }

    #[test]
    fn agent_projects_and_recent_folders_join_without_sessions() {
        let mut f = fixture();
        f.known = vec![
            known("/home/u/w/other", false),
            known("/home/u/w/gone", true),
            known("/home/u/w/muqun/app", false),
        ];
        f.recent = vec![PathBuf::from("/srv/site")];
        f.dirs.insert(
            "/home/u/w/old".into(),
            DirInfo {
                root: "/home/u/w/old".into(),
                kind: ProjectKind::Plain,
                branch: None,
                exists: false,
            },
        );
        f.sessions.push(listed("z", Some("/home/u/w/old")));
        let projects = f.derive();
        let all = ids(&projects);
        assert!(all.contains(&"/home/u/w/other") && all.contains(&"/srv/site"));
        assert!(!all.contains(&"/home/u/w/gone"), "gone and empty: left out");
        assert_eq!(
            all.iter()
                .filter(|id| **id == "/home/u/w/muqun/app")
                .count(),
            1,
            "a known project with sessions is one project"
        );
        assert!(
            !all.contains(&"/home/u/w/old"),
            "a missing folder folds into Other"
        );
        let folded = projects.iter().find(|p| p.is_other()).expect("other");
        assert!(folded.sessions.contains(&"z".into()));
        let other = projects.iter().position(|p| p.id == "/home/u/w/other");
        let with_sessions = projects
            .iter()
            .position(|p| p.id == "/home/u/w/muqun/gateway");
        assert!(
            with_sessions < other,
            "projects with sessions before those without"
        );
        assert_eq!(projects.last().unwrap().id, "", "no-folder group last");
    }

    fn tree(projects: &[Project], query: &str, open: &HashMap<String, bool>) -> Vec<TreeRow> {
        flatten(
            projects,
            &TreeOptions {
                query,
                open,
                full: &HashSet::new(),
                default_open: 3,
                per_project: 5,
            },
            |asid| Some(format!("title {asid}")),
        )
    }

    fn many(projects: usize, sessions: usize) -> Vec<Project> {
        (0..projects)
            .map(|p| Project {
                id: format!("/p{p}"),
                root: format!("/p{p}").into(),
                display: (String::new(), format!("p{p}")),
                kind: ProjectKind::Plain,
                pinned: false,
                hidden: false,
                missing: false,
                last_active: p,
                branch: None,
                sessions: (0..sessions).map(|s| format!("s{p}-{s}")).collect(),
            })
            .collect()
    }

    #[test]
    fn three_projects_open_with_five_sessions_and_more() {
        let projects = many(5, 8);
        let rows = tree(&projects, "", &HashMap::new());
        let opened: Vec<bool> = rows
            .iter()
            .filter_map(|r| match r {
                TreeRow::Project { open, .. } => Some(*open),
                _ => None,
            })
            .collect();
        assert_eq!(opened, [true, true, true, false, false]);
        let sessions = rows
            .iter()
            .filter(|r| matches!(r, TreeRow::Session { .. }))
            .count();
        assert_eq!(sessions, 15);
        assert!(rows.contains(&TreeRow::More {
            project: "/p0".into(),
            hidden: 3
        }));
        assert_eq!(
            rows[0],
            TreeRow::Project {
                id: "/p0".into(),
                open: true
            }
        );
        assert_eq!(rows.iter().filter(|r| **r == TreeRow::Gap).count(), 4);

        let mut open = HashMap::new();
        open.insert("/p0".to_string(), false);
        open.insert("/p4".to_string(), true);
        let rows = tree(&projects, "", &open);
        assert_eq!(rows[1], TreeRow::Gap, "p0 closed: straight on to p1");
        assert!(rows.contains(&TreeRow::Session {
            asid: "s4-0".into(),
            project: "/p4".into()
        }));
    }

    #[test]
    fn search_filters_projects_and_sessions_and_hidden_ones_only_show_in_search() {
        let mut projects = many(3, 8);
        projects[2].hidden = true;
        let rows = tree(&projects, "", &HashMap::new());
        assert!(!rows
            .iter()
            .any(|r| matches!(r, TreeRow::Project { id, .. } if id == "/p2")));
        let rows = tree(&projects, "s1-7", &HashMap::new());
        assert_eq!(
            rows,
            [
                TreeRow::Project {
                    id: "/p1".into(),
                    open: true
                },
                TreeRow::Session {
                    asid: "s1-7".into(),
                    project: "/p1".into()
                },
            ]
        );
        let rows = tree(&projects, "p2", &HashMap::new());
        assert_eq!(rows.len(), 9, "a named project shows all its sessions");
    }

    #[test]
    fn heads_name_a_branch_or_a_short_hash() {
        assert_eq!(
            head_branch("ref: refs/heads/feat/x\n").as_deref(),
            Some("feat/x")
        );
        assert_eq!(
            head_branch("957e5003c1a2b4d5e6f708192a3b4c5d6e7f8091\n").as_deref(),
            Some("957e500")
        );
        assert_eq!(head_branch("garbage"), None);
    }

    #[test]
    fn resolve_finds_the_checkout_and_its_worktrees() {
        let base = std::env::temp_dir().join(format!("mq-projects-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        let tree = base.join("wt");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::write(
            repo.join(".git/worktrees/wt/HEAD"),
            "ref: refs/heads/topic\n",
        )
        .unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        let deep = resolve(&repo.join("src/deep"));
        assert_eq!(deep.root, repo);
        assert_eq!(deep.kind, ProjectKind::Git);
        assert_eq!(deep.branch.as_deref(), Some("main"));
        let wt = resolve(&tree);
        assert_eq!(wt.root, tree);
        assert_eq!(wt.branch.as_deref(), Some("topic"));
        let gone = resolve(&base.join("nope"));
        assert!(!gone.exists);
        // `plain` may sit inside a checkout on the machine running the
        // tests (the temp dir rarely is); only its existence is certain.
        assert!(resolve(&plain).exists);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scratch_and_overflow_fold_into_other() {
        assert!(is_scratch(Path::new("/tmp/qa"), false, None));
        assert!(is_scratch(
            Path::new("/home/u/.cache/x"),
            false,
            Some(Path::new("/home/u"))
        ));
        assert!(is_scratch(Path::new("/home/u/w/gone"), true, None));
        assert!(!is_scratch(
            Path::new("/home/u/w/app"),
            false,
            Some(Path::new("/home/u"))
        ));

        let mut f = Fixture {
            sessions: (0..10)
                .map(|i| listed(&format!("s{i}"), Some(&format!("/home/u/w/p{i}"))))
                .collect(),
            dirs: HashMap::new(),
            known: vec![],
            recent: vec![],
            prefs: BTreeMap::new(),
            session_pins: vec![],
        };
        f.sessions.push(listed("tmp", Some("/tmp/opencode/qa")));
        f.prefs.insert(
            "/home/u/w/p9".into(),
            ProjectPrefs {
                pinned: true,
                ..ProjectPrefs::default()
            },
        );
        let projects = f.derive();
        let other = projects.iter().find(|p| p.is_other()).expect("other");
        assert!(other.sessions.contains(&"tmp".into()));
        assert!(
            projects
                .iter()
                .filter(|p| !p.is_other() && !p.is_no_folder())
                .count()
                <= Project::MAX_PRIMARY,
            "at most eight ordinary projects"
        );
        assert!(
            projects.iter().any(|p| p.id == "/home/u/w/p9" && p.pinned),
            "pinned keeps a seat"
        );

        // A start-page session pin soft-boosts its project the same way.
        let f = Fixture {
            sessions: (0..10)
                .map(|i| listed(&format!("s{i}"), Some(&format!("/home/u/w/p{i}"))))
                .collect(),
            dirs: HashMap::new(),
            known: vec![],
            recent: vec![],
            prefs: BTreeMap::new(),
            session_pins: vec!["s9".into()],
        };
        let projects = f.derive();
        assert!(
            projects.iter().any(|p| p.id == "/home/u/w/p9" && p.pinned),
            "session pin keeps its project seated"
        );
    }
}
