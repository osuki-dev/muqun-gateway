//! `<config>/config.json`: what the owner chose, in one file.
//!
//! Every struct is `#[serde(default)]`, so a file written by an older build --
//! or one the owner trimmed by hand -- still loads, with the missing keys at
//! their defaults. The OpenCode block is the gateway's own
//! [`OpencodeConfig`], serialized exactly as the gateway's `config.json`
//! names it, so the two files read the same.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use muqun_gateway::agents::compose::RuntimeSettings;
use muqun_gateway::agents::runtime::LocalAgentConfigs;
use muqun_gateway::agents::{
    AntigravityConfig, ClaudeConfig, CodexConfig, CursorConfig, DeepseekConfig, FxConfig,
    OpencodeConfig, PiConfig, QoderConfig, T3Config,
};
use serde::{Deserialize, Serialize};

use crate::model::projects::ProjectPrefs;

pub const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    pub agents: AgentConfigs,
    pub general: General,
    pub appearance: Appearance,
    pub ui: UiPrefs,
    pub window: WindowGeometry,
    pub notifications: Notifications,
    /// Dictation's speech service (the API key lives in the keyring).
    pub voice: VoiceConfig,
}

/// How dictation transcribes (the App's Settings → Voice).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceMode {
    /// The whole recording to an OpenAI-compatible `/v1/audio/transcriptions`.
    #[default]
    File,
    /// Streamed to an OpenAI-Realtime-compatible WebSocket, words as you speak.
    Realtime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub mode: VoiceMode,
    /// The service's endpoint: `https://…` after recording, `wss://…` realtime.
    pub url: String,
    /// Empty: the service's own default.
    pub model: String,
    /// `auto`, or a language tag (`en`, `zh-CN`, …).
    pub language: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            mode: VoiceMode::File,
            url: String::new(),
            model: String::new(),
            language: "auto".into(),
        }
    }
}

impl VoiceConfig {
    /// The App's presets, shown as placeholders and followed when the mode
    /// changes while the URL is still the preset.
    pub fn preset(mode: VoiceMode) -> (&'static str, &'static str) {
        match mode {
            VoiceMode::File => (
                "https://api.openai.com/v1/audio/transcriptions",
                "gpt-4o-mini-transcribe",
            ),
            VoiceMode::Realtime => (
                "wss://api.openai.com/v1/realtime?intent=transcription",
                "gpt-live-transcribe",
            ),
        }
    }

    /// The URL is one this mode can use (`https` / `wss`, no credentials).
    pub fn usable(&self) -> bool {
        let url = self.url.trim();
        let scheme = match self.mode {
            VoiceMode::File => "https://",
            VoiceMode::Realtime => "wss://",
        };
        url.starts_with(scheme) && url.len() > scheme.len() && !url[scheme.len()..].contains('@')
    }
}

/// System notifications for sessions that are not on screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    /// An approval, a question or a finished turn raises a system
    /// notification (the App's "Gateway notifications"). On by default.
    pub enabled: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Every agent the gateway drives, under the names its `config.json` uses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfigs {
    pub codex: CodexConfig,
    pub opencode: OpencodeConfig,
    pub deepseek: DeepseekConfig,
    pub t3: T3Config,
    pub fx: FxConfig,
    pub qoder: QoderConfig,
    pub pi: PiConfig,
    pub claude: ClaudeConfig,
    pub antigravity: AntigravityConfig,
    pub cursor: CursorConfig,
}

impl AgentConfigs {
    /// The runtime's settings, as configured.
    pub fn runtime_settings(&self) -> RuntimeSettings {
        RuntimeSettings {
            opencode: self.opencode.clone(),
            deepseek: self.deepseek.clone(),
            t3: self.t3.clone(),
            local: LocalAgentConfigs {
                codex: self.codex.clone(),
                fx: self.fx.clone(),
                qoder: self.qoder.clone(),
                pi: self.pi.clone(),
                claude: self.claude.clone(),
                antigravity: self.antigravity.clone(),
                cursor: self.cursor.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Where "New session" starts when nothing was remembered for the agent.
    /// Absent: the home directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<PathBuf>,
    /// The folder each agent last started a session in, by agent id.
    pub last_cwd: HashMap<String, PathBuf>,
    /// The agent the last session was started with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_agent: Option<String>,
    /// The model last chosen for an agent in a folder, by [`choice_key`]
    /// (the composer preselects it next time).
    pub last_model: HashMap<String, String>,
    /// The mode last chosen, likewise.
    pub last_mode: HashMap<String, String>,
    /// Unsent composer text: the start page under [`START_DRAFT`], each
    /// session under its id. Survives switching sessions and restarts.
    pub drafts: HashMap<String, String>,
    /// Pinned sessions (ids), newest pin first: the start page's recent
    /// list shows them first.
    pub pinned: Vec<String>,
    pub language: Language,
    /// The command "open in editor" will run, for later (`code`, `zed`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor_command: Option<String>,
    /// The top bar's "Open in …": the editor used last.
    pub editor: crate::platform::editors::Editor,
    /// Placeholder: closing the window will hide it to the tray instead of
    /// quitting. Stored, not acted on yet (there is no tray in P0).
    pub close_to_tray: bool,
}

/// The draft key of the start page.
pub const START_DRAFT: &str = "start";
/// The most drafts kept; the oldest-written are not tracked, so past this the
/// session drafts of a few arbitrary sessions go (the start page's never).
const MAX_DRAFTS: usize = 64;

/// What "an agent in a folder" is called in [`General::last_model`] and
/// [`General::last_mode`].
pub fn choice_key(agent: &str, cwd: &str) -> String {
    format!("{agent}\u{1f}{cwd}")
}

impl General {
    /// The folder to preselect for `agent`: the one it last used, else the
    /// configured default, else `~`.
    pub fn initial_cwd(&self, agent: &str) -> PathBuf {
        self.last_cwd
            .get(agent)
            .or(self.default_cwd.as_ref())
            .cloned()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

impl Default for Language {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl Language {
    /// Simplified Chinese when the session's locale says Chinese, else English.
    pub fn from_environment() -> Self {
        let lang = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
            .unwrap_or_default();
        Self::from_locale_tag(&lang)
    }

    fn from_locale_tag(tag: &str) -> Self {
        if tag.to_ascii_lowercase().starts_with("zh") {
            Self::ZhCn
        } else {
            Self::En
        }
    }

    /// The gateway's i18n locale for the same language.
    #[allow(dead_code)]
    pub fn locale(self) -> muqun_gateway::platform::i18n::Locale {
        use muqun_gateway::platform::i18n::Locale;
        match self {
            Self::ZhCn => Locale::from_code("zh-CN").unwrap_or_default(),
            Self::En => Locale::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub scheme: Scheme,
    pub theme_id: String,
    /// "Reduce motion" (architecture §6.1 red line 6): the middle and large
    /// motion tiers become short fades.
    pub reduced_motion: bool,
    /// "Reduce transparency" (§6.1d): glass surfaces fall back to solid.
    pub reduced_transparency: bool,
    pub density: Density,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            scheme: Scheme::System,
            theme_id: crate::theme::DEFAULT_THEME_ID.to_string(),
            reduced_motion: false,
            reduced_transparency: false,
            density: Density::Comfortable,
        }
    }
}

/// Row heights and spacing, scaled together (§6.1d).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    Light,
    Dark,
    #[default]
    System,
}

/// Window furniture the owner arranged and expects back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPrefs {
    /// The sidebar's width in logical pixels.
    pub sidebar_width: f32,
    /// The right panel's preferred width (Queue / Git tabs). Clamped further
    /// at layout time so the left column and main stay usable.
    #[serde(default = "UiPrefs::right_default")]
    pub right_width: f32,
    /// Pins, hidden projects and names given to projects, by project root
    /// (`model/projects.rs`). Only projects with a choice are kept.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, ProjectPrefs>,
    /// Sessions open side by side when the window closed, in tile order
    /// (`ui/tiles.rs`); empty when one session filled the pane.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tiles: Vec<String>,
    /// Where the split's first column ends, 0.25..=0.75 of its width.
    #[serde(default = "UiPrefs::tile_split_default")]
    pub tile_split: f32,
}

impl UiPrefs {
    pub const SIDEBAR_DEFAULT: f32 = 260.0;
    pub const SIDEBAR_MIN: f32 = 220.0;
    pub const SIDEBAR_MAX: f32 = 360.0;
    pub const RIGHT_DEFAULT: f32 = 320.0;
    pub const RIGHT_MIN: f32 = 240.0;
    pub const RIGHT_MAX: f32 = 720.0;
    /// Main column must keep at least this much (timeline + composer).
    #[allow(dead_code)]
    pub const MAIN_MIN: f32 = 420.0;

    fn right_default() -> f32 {
        Self::RIGHT_DEFAULT
    }

    fn tile_split_default() -> f32 {
        0.5
    }

    /// The stored split, forced into range.
    pub fn tile_split(&self) -> f32 {
        if self.tile_split.is_finite() {
            self.tile_split.clamp(0.25, 0.75)
        } else {
            0.5
        }
    }

    /// The stored width, forced into the range the layout allows (a hand
    /// edited or NaN value never yields a broken window).
    pub fn sidebar(&self) -> f32 {
        if self.sidebar_width.is_finite() {
            self.sidebar_width
                .clamp(Self::SIDEBAR_MIN, Self::SIDEBAR_MAX)
        } else {
            Self::SIDEBAR_DEFAULT
        }
    }

    /// Preferred right-panel width before the live window clamp.
    pub fn right(&self) -> f32 {
        if self.right_width.is_finite() {
            self.right_width.clamp(Self::RIGHT_MIN, Self::RIGHT_MAX)
        } else {
            Self::RIGHT_DEFAULT
        }
    }
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            sidebar_width: Self::SIDEBAR_DEFAULT,
            right_width: Self::RIGHT_DEFAULT,
            projects: BTreeMap::new(),
            tiles: Vec::new(),
            tile_split: 0.5,
        }
    }
}

/// One change the owner made, applied by the core thread in the order the
/// UI sent them (see `model/projection.rs`). Pure data, so applying it is a
/// plain function ([`DesktopConfig::apply`]) with tests.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigEdit {
    #[allow(dead_code)]
    Window(WindowGeometry),
    SidebarWidth(f32),
    RightWidth(f32),
    /// The sessions open side by side and where the split sits.
    Tiles {
        asids: Vec<String>,
        split: f32,
    },
    /// What the owner chose for one project (pin, hide, name), by root.
    Project {
        root: String,
        prefs: ProjectPrefs,
    },
    /// The editor "Open in …" uses.
    Editor(crate::platform::editors::Editor),
    Scheme(Scheme),
    Language(Language),
    Density(Density),
    ReducedMotion(bool),
    ReducedTransparency(bool),
    Notifications(bool),
    Voice(VoiceConfig),
    EditorCommand(Option<String>),
    /// Where "New session" starts for an agent with no remembered folder.
    DefaultCwd(Option<PathBuf>),
    AgentEnabled {
        agent: String,
        on: bool,
    },
    /// What the composer chose for an agent in a folder. `None` clears.
    Remember {
        agent: String,
        cwd: String,
        model: Option<String>,
        mode: Option<String>,
    },
    /// Pin the session, or unpin it when it is pinned.
    TogglePin(String),
    /// The unsent text under a draft key; empty removes it.
    Draft {
        key: String,
        text: String,
    },
    /// One field of one agent's block, as the owner typed it. An invalid
    /// value changes nothing (the settings page has already said why).
    AgentValue {
        agent: String,
        field: crate::agent_fields::Field,
        value: String,
    },
}

impl DesktopConfig {
    /// Fold one edit in.
    pub fn apply(&mut self, edit: ConfigEdit) {
        match edit {
            ConfigEdit::Window(geometry) => self.window = geometry,
            ConfigEdit::SidebarWidth(width) => self.ui.sidebar_width = width,
            ConfigEdit::RightWidth(width) => self.ui.right_width = width,
            ConfigEdit::Tiles { asids, split } => {
                self.ui.tiles = asids;
                self.ui.tile_split = split;
            }
            ConfigEdit::Project { root, prefs } => {
                if prefs.is_default() {
                    self.ui.projects.remove(&root);
                } else {
                    self.ui.projects.insert(root, prefs);
                }
            }
            ConfigEdit::Editor(editor) => self.general.editor = editor,
            ConfigEdit::Scheme(scheme) => self.appearance.scheme = scheme,
            ConfigEdit::Language(language) => self.general.language = language,
            ConfigEdit::Density(density) => self.appearance.density = density,
            ConfigEdit::ReducedMotion(on) => self.appearance.reduced_motion = on,
            ConfigEdit::ReducedTransparency(on) => self.appearance.reduced_transparency = on,
            ConfigEdit::Notifications(on) => self.notifications.enabled = on,
            ConfigEdit::Voice(voice) => self.voice = voice,
            ConfigEdit::EditorCommand(command) => {
                self.general.editor_command = command.filter(|c| !c.trim().is_empty());
            }
            ConfigEdit::DefaultCwd(cwd) => self.general.default_cwd = cwd,
            ConfigEdit::Remember {
                agent,
                cwd,
                model,
                mode,
            } => {
                let key = choice_key(&agent, &cwd);
                let g = &mut self.general;
                match model {
                    Some(m) => g.last_model.insert(key.clone(), m),
                    None => g.last_model.remove(&key),
                };
                match mode {
                    Some(m) => g.last_mode.insert(key, m),
                    None => g.last_mode.remove(&key),
                };
            }
            ConfigEdit::TogglePin(asid) => {
                let pinned = &mut self.general.pinned;
                match pinned.iter().position(|p| *p == asid) {
                    Some(at) => {
                        pinned.remove(at);
                    }
                    None => pinned.insert(0, asid),
                }
            }
            ConfigEdit::Draft { key, text } => {
                let drafts = &mut self.general.drafts;
                if text.trim().is_empty() {
                    drafts.remove(&key);
                } else {
                    drafts.insert(key, text);
                    while drafts.len() > MAX_DRAFTS {
                        let Some(victim) = drafts.keys().find(|k| *k != START_DRAFT).cloned()
                        else {
                            break;
                        };
                        drafts.remove(&victim);
                    }
                }
            }
            ConfigEdit::AgentEnabled { agent, on } => self.agents.set_enabled(&agent, on),
            ConfigEdit::AgentValue {
                agent,
                field,
                value,
            } => {
                if let Err(err) = self.agents.set_value(&agent, field, &value) {
                    tracing::debug!(agent, ?field, %err, "an invalid value was not applied");
                }
            }
        }
    }
}

/// Logical pixels. `x`/`y` are absent until a position is known: Wayland
/// never tells a client where its window is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowGeometry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    pub w: u32,
    pub h: u32,
    pub maximized: bool,
}

impl Default for WindowGeometry {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            w: 1200,
            h: 800,
            maximized: false,
        }
    }
}

impl WindowGeometry {
    /// The smallest window the layout is designed for.
    pub const MIN: (u32, u32) = (720, 480);

    /// This geometry with a size no smaller than [`Self::MIN`].
    pub fn clamped(mut self) -> Self {
        self.w = self.w.max(Self::MIN.0);
        self.h = self.h.max(Self::MIN.1);
        self
    }
}

impl DesktopConfig {
    /// Read `path`. A missing file is the defaults; a file that does not
    /// parse is an error, so a typo is reported instead of silently reset.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|err| anyhow::anyhow!("{} is not valid: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(anyhow::anyhow!("cannot read {}: {err}", path.display())),
        }
    }

    /// Write `path` atomically: a sibling temporary file, flushed, then
    /// renamed over the old one, so a crash mid-write leaves the previous
    /// file and never half of a new one.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
        std::fs::create_dir_all(dir)?;
        let mut json = serde_json::to_vec_pretty(self)?;
        json.push(b'\n');
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("config"),
            std::process::id()
        ));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&json)?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(|err| anyhow::anyhow!("cannot write {}: {err}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muqun-desktop-config-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_object_loads_as_the_defaults() {
        let config: DesktopConfig = serde_json::from_str("{}").unwrap();
        assert!(config.agents.opencode.enabled && config.agents.opencode.autostart);
        assert!(config.agents.codex.enabled);
        assert!(!config.agents.deepseek.enabled, "the gateway's default");
        assert_eq!(config.appearance.scheme, Scheme::System);
        assert_eq!(config.appearance.theme_id, "cover-courier");
        assert!(!config.appearance.reduced_motion && !config.appearance.reduced_transparency);
        assert_eq!(config.appearance.density, Density::Comfortable);
        assert_eq!(config.window, WindowGeometry::default());
        assert!(
            config.notifications.enabled,
            "notifications are on until turned off"
        );
    }

    #[test]
    fn partial_blocks_keep_their_other_defaults() {
        let config: DesktopConfig = serde_json::from_str(
            r#"{"agents":{"opencode":{"autostart":false}},"general":{"language":"zh-CN"},"window":{"w":900}}"#,
        )
        .unwrap();
        assert!(config.agents.opencode.enabled);
        assert!(!config.agents.opencode.autostart);
        assert!(
            config.agents.claude.enabled,
            "an absent agent keeps its default"
        );
        assert_eq!(config.general.language, Language::ZhCn);
        assert_eq!(config.window.w, 900);
        assert_eq!(config.window.h, 800);
    }

    #[test]
    fn choices_and_drafts_are_remembered_and_cleared() {
        let mut config = DesktopConfig::default();
        config.apply(ConfigEdit::Remember {
            agent: "codex".into(),
            cwd: "/work/a".into(),
            model: Some("gpt".into()),
            mode: None,
        });
        let key = choice_key("codex", "/work/a");
        assert_eq!(
            config.general.last_model.get(&key).map(String::as_str),
            Some("gpt")
        );
        assert!(!config.general.last_mode.contains_key(&key));
        config.apply(ConfigEdit::Draft {
            key: START_DRAFT.into(),
            text: "hello".into(),
        });
        assert_eq!(config.general.drafts[START_DRAFT], "hello");
        config.apply(ConfigEdit::Draft {
            key: START_DRAFT.into(),
            text: "  ".into(),
        });
        assert!(
            config.general.drafts.is_empty(),
            "an empty draft is removed"
        );
        for n in 0..(MAX_DRAFTS + 10) {
            config.apply(ConfigEdit::Draft {
                key: format!("s{n}"),
                text: "x".into(),
            });
        }
        config.apply(ConfigEdit::Draft {
            key: START_DRAFT.into(),
            text: "keep".into(),
        });
        assert!(config.general.drafts.len() <= MAX_DRAFTS);
        assert!(config.general.drafts.contains_key(START_DRAFT));
    }

    #[test]
    fn pinning_toggles() {
        let mut config = DesktopConfig::default();
        config.apply(ConfigEdit::TogglePin("a".into()));
        config.apply(ConfigEdit::TogglePin("b".into()));
        assert_eq!(config.general.pinned, ["b", "a"]);
        config.apply(ConfigEdit::TogglePin("b".into()));
        assert_eq!(config.general.pinned, ["a"]);
    }

    #[test]
    fn save_then_load_round_trips_and_leaves_no_temporary_file() {
        let dir = scratch();
        let path = dir.join(CONFIG_FILE);
        let mut config = DesktopConfig::default();
        config
            .general
            .last_cwd
            .insert("opencode".into(), PathBuf::from("/tmp/x"));
        config.appearance.scheme = Scheme::Dark;
        config.save(&path).unwrap();
        config.window.w = 1000;
        config.save(&path).unwrap();
        let loaded = DesktopConfig::load(&path).unwrap();
        assert_eq!(loaded.window.w, 1000);
        assert_eq!(loaded.appearance.scheme, Scheme::Dark);
        assert_eq!(
            loaded.general.last_cwd.get("opencode"),
            Some(&PathBuf::from("/tmp/x"))
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != CONFIG_FILE)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_a_broken_one_is_an_error() {
        let dir = scratch();
        let path = dir.join(CONFIG_FILE);
        assert!(DesktopConfig::load(&path).is_ok());
        std::fs::write(&path, "{ not json").unwrap();
        assert!(DesktopConfig::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_initial_folder_prefers_the_remembered_one() {
        let mut general = General {
            default_cwd: Some(PathBuf::from("/srv")),
            ..General::default()
        };
        assert_eq!(general.initial_cwd("opencode"), PathBuf::from("/srv"));
        general
            .last_cwd
            .insert("opencode".into(), PathBuf::from("/work"));
        assert_eq!(general.initial_cwd("opencode"), PathBuf::from("/work"));
        general.default_cwd = None;
        assert_eq!(general.initial_cwd("codex"), dirs::home_dir().unwrap());
    }

    #[test]
    fn chinese_locales_pick_simplified_chinese() {
        assert_eq!(Language::from_locale_tag("zh_CN.UTF-8"), Language::ZhCn);
        assert_eq!(Language::from_locale_tag("en_US.UTF-8"), Language::En);
        assert_eq!(Language::from_locale_tag(""), Language::En);
    }

    #[test]
    fn edits_change_only_what_they_name() {
        use crate::agent_fields::Field;
        let mut config = DesktopConfig::default();
        config.apply(ConfigEdit::SidebarWidth(333.0));
        config.apply(ConfigEdit::Project {
            root: "/w/x".into(),
            prefs: ProjectPrefs {
                pinned: true,
                ..ProjectPrefs::default()
            },
        });
        config.apply(ConfigEdit::Project {
            root: "/w/y".into(),
            prefs: ProjectPrefs::default(),
        });
        config.apply(ConfigEdit::Editor(crate::platform::editors::Editor::Zed));
        config.apply(ConfigEdit::Language(Language::ZhCn));
        config.apply(ConfigEdit::Scheme(Scheme::Dark));
        config.apply(ConfigEdit::Density(Density::Compact));
        config.apply(ConfigEdit::ReducedMotion(true));
        config.apply(ConfigEdit::ReducedTransparency(true));
        config.apply(ConfigEdit::DefaultCwd(Some(PathBuf::from("/srv"))));
        config.apply(ConfigEdit::EditorCommand(Some("  ".into())));
        config.apply(ConfigEdit::AgentEnabled {
            agent: "fx".into(),
            on: false,
        });
        config.apply(ConfigEdit::AgentValue {
            agent: "claude".into(),
            field: Field::IdleTimeout,
            value: "90".into(),
        });
        // An invalid value is dropped, not stored.
        config.apply(ConfigEdit::AgentValue {
            agent: "codex".into(),
            field: Field::Binary,
            value: "codex".into(),
        });
        assert_eq!(config.ui.sidebar_width, 333.0);
        assert!(config.ui.projects["/w/x"].pinned);
        assert!(
            !config.ui.projects.contains_key("/w/y"),
            "a project with nothing chosen is not kept"
        );
        assert_eq!(config.general.editor, crate::platform::editors::Editor::Zed);
        assert_eq!(config.general.language, Language::ZhCn);
        assert_eq!(config.appearance.scheme, Scheme::Dark);
        assert_eq!(config.appearance.density, Density::Compact);
        assert!(config.appearance.reduced_motion);
        assert!(config.appearance.reduced_transparency);
        assert_eq!(config.general.default_cwd, Some(PathBuf::from("/srv")));
        assert_eq!(config.general.editor_command, None, "blank is unset");
        assert!(!config.agents.fx.enabled);
        assert_eq!(config.agents.claude.idle_timeout_secs, 90);
        assert!(config.agents.codex.binary.is_none());
        assert!(
            config.agents.opencode.enabled,
            "untouched agents keep their defaults"
        );
    }

    #[test]
    fn the_sidebar_width_is_kept_inside_what_the_layout_allows() {
        let width = |w: f32| {
            UiPrefs {
                sidebar_width: w,
                ..UiPrefs::default()
            }
            .sidebar()
        };
        assert_eq!(width(10.0), UiPrefs::SIDEBAR_MIN);
        assert_eq!(width(9000.0), UiPrefs::SIDEBAR_MAX);
        assert_eq!(width(f32::NAN), UiPrefs::SIDEBAR_DEFAULT);
        assert_eq!(width(320.0), 320.0);
    }

    #[test]
    fn an_old_file_without_the_new_blocks_still_loads() {
        // `grouping` and `agent_filter` were the v1 sidebar's; they are
        // ignored now, not an error.
        let config: DesktopConfig = serde_json::from_str(
            r#"{"general":{"language":"en"},"ui":{"grouping":"agent","agent_filter":"codex"}}"#,
        )
        .unwrap();
        assert_eq!(config.ui.sidebar_width, UiPrefs::SIDEBAR_DEFAULT);
        assert_eq!(config.ui.right_width, UiPrefs::RIGHT_DEFAULT);
        assert!(config.ui.projects.is_empty());
        assert!(!config.general.close_to_tray);
        assert_eq!(
            config.general.editor,
            crate::platform::editors::Editor::VsCode
        );
        let config: DesktopConfig = serde_json::from_str(
            r#"{"ui":{"projects":{"/w/x":{"pinned":true,"name":"X"}}},"general":{"editor":"zed"}}"#,
        )
        .unwrap();
        assert_eq!(config.ui.projects["/w/x"].name.as_deref(), Some("X"));
        assert_eq!(config.general.editor, crate::platform::editors::Editor::Zed);
    }
}
