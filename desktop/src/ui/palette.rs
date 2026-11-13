//! The command palette (Ctrl/Cmd+K): one frosted field over the window that
//! jumps to any session and runs any action -- new sessions by agent and
//! folder, the split, the inspector, every page and Settings tab, theme packs,
//! light/dark, dictation, and every keymap command with its chord on the
//! right. Ctrl/Cmd+P opens it on sessions alone (quick open). Fuzzy matching
//! with the matched letters lit, ↑/↓ and Enter, recent commands first. It
//! opens on a spring and settles without one under reduce motion.

use std::path::PathBuf;

use rune::{col, el, rich, row, text, El, Span, Spring, TextEdit, Transition, Tx};
use rune_kit::{Kit, TextInput};

use super::copy::{tr, tr_ref};
use super::keymap::{self, Action};
use super::{Mode, UiState};
use crate::config::{ConfigEdit, Scheme};
use crate::model::commands::Command;

/// Rows shown at once; the rest scroll.
const SHOWN: usize = 9;
/// Recent commands remembered.
pub const RECENT: usize = 8;

/// The open palette.
pub struct Palette {
    pub query: TextEdit,
    pub selected: usize,
    /// Quick open: sessions only, by title and folder.
    pub sessions: bool,
}

impl Palette {
    pub fn new(sessions: bool) -> Self {
        let mut query = TextEdit::new("");
        query.focused = true;
        Self {
            query,
            selected: 0,
            sessions,
        }
    }
}

/// What a row does when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Session(String),
    NewSession {
        agent: Option<String>,
        cwd: Option<PathBuf>,
    },
    Go(Mode),
    Settings(super::SettingsTab),
    Split,
    Unsplit,
    Inspector,
    Git,
    Theme(String),
    ToggleScheme,
    Dictation,
    /// A keymap command (`ui/keymap.rs`).
    Key(Action),
    /// Open a plugin in a specific surface (`Page`, `Tile`, `Drawer`).
    PluginSurface {
        plugin_id: String,
        surface: crate::plugin::PluginSlot,
    },
    /// Execute a command contributed by a registered plugin.
    PluginCommand {
        plugin_id: String,
        command_id: String,
    },
}

impl Act {
    /// The keymap command this row runs, whose chord it shows.
    fn shortcut(&self) -> Option<Action> {
        Some(match self {
            Self::NewSession {
                agent: None,
                cwd: None,
            } => Action::NewSession,
            Self::Go(Mode::Home) => Action::Home,
            Self::Go(Mode::Inbox) => Action::Inbox,
            Self::Go(Mode::Tasks) => Action::Tasks,
            Self::Split => Action::AddSplit,
            Self::Inspector => Action::ToggleInspector,
            Self::Git => Action::ToggleGit,
            Self::Dictation => Action::Dictation,
            Self::Key(action) => *action,
            _ => return None,
        })
    }

    /// A stable id for "recent commands" (sessions by id, actions by kind).
    fn id(&self) -> String {
        match self {
            Self::Session(asid) => format!("session:{asid}"),
            Self::NewSession { agent, cwd } => format!(
                "new:{}:{}",
                agent.as_deref().unwrap_or(""),
                cwd.as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            ),
            Self::Go(mode) => format!("go:{mode:?}"),
            Self::Settings(tab) => format!("settings:{tab:?}"),
            Self::Split => "split".into(),
            Self::Unsplit => "unsplit".into(),
            Self::Inspector => "inspector".into(),
            Self::Git => "git".into(),
            Self::Theme(id) => format!("theme:{id}"),
            Self::ToggleScheme => "scheme".into(),
            Self::Dictation => "dictation".into(),
            Self::Key(action) => format!("key:{action:?}"),
            Self::PluginSurface { plugin_id, surface } => {
                format!("plugin-surface:{plugin_id}:{surface:?}")
            }
            Self::PluginCommand {
                plugin_id,
                command_id,
            } => format!("plugin-cmd:{plugin_id}:{command_id}"),
        }
    }
}

/// One row: what it says, what it does.
#[derive(Debug, Clone)]
pub struct Entry {
    pub label: String,
    pub detail: String,
    /// A word on the right: "Session", "Action", a shortcut.
    pub hint: String,
    pub act: Act,
    /// Agent mark to draw, for sessions and new sessions.
    pub agent: Option<String>,
}

/// A fuzzy match: letters of `query` in order in `text`, case-insensitive.
/// Scores consecutive runs and word starts; returns the matched char indices.
pub fn fuzzy(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    let t: Vec<char> = text.chars().collect();
    let lower: Vec<char> = t
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let mut hits = Vec::with_capacity(q.len());
    let mut score = 0i32;
    let mut qi = 0;
    let mut last: Option<usize> = None;
    for (i, c) in lower.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            let start = i == 0
                || !t[i - 1].is_alphanumeric()
                || (t[i].is_uppercase() && t[i - 1].is_lowercase());
            score += 1;
            if last == Some(i.wrapping_sub(1)) {
                score += 5;
            }
            if start {
                score += 8;
            }
            if i == 0 {
                score += 4;
            }
            hits.push(i);
            last = Some(i);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    // Tighter matches rank above scattered ones.
    let span = hits.last().unwrap_or(&0) - hits.first().unwrap_or(&0);
    score -= (span as i32 - q.len() as i32).max(0) / 3;
    Some((score, hits))
}

fn folder_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.is_empty() {
        path.to_string()
    } else {
        name.to_string()
    }
}

/// Everything the palette can do right now.
pub fn entries(s: &UiState) -> Vec<Entry> {
    let language = s.cfg.general.language;
    let mut out = Vec::new();
    let action = |label: String, hint: &str, act: Act| Entry {
        label,
        detail: String::new(),
        hint: hint.to_string(),
        act,
        agent: None,
    };
    // Sessions, most recent first (the list is kept in that order).
    for info in &s.sessions {
        let folder = info
            .directory
            .as_deref()
            .map(folder_name)
            .unwrap_or_default();
        out.push(Entry {
            label: super::title_of(info),
            detail: [s.agent_name(&info.agent_id), folder]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            hint: tr(language, "Session"),
            act: Act::Session(info.asid.0.clone()),
            agent: Some(info.agent_id.clone()),
        });
    }
    if s.palette.as_ref().is_some_and(|p| p.sessions) {
        return out;
    }
    let new = tr(language, "New session");
    out.push(action(
        new.clone(),
        &keymap::label(Action::NewSession),
        Act::NewSession {
            agent: None,
            cwd: None,
        },
    ));
    let usable: Vec<_> = s.agents.iter().filter(|a| a.enabled).collect();
    let mut folders: Vec<PathBuf> = s.cfg.general.last_cwd.values().cloned().collect();
    folders.sort();
    folders.dedup();
    for agent in &usable {
        let name = s.agent_name(&agent.id);
        out.push(Entry {
            label: tr(language, &format!("New {name} session")),
            detail: String::new(),
            hint: tr(language, "Action"),
            act: Act::NewSession {
                agent: Some(agent.id.clone()),
                cwd: None,
            },
            agent: Some(agent.id.clone()),
        });
        for folder in folders.iter().take(6) {
            let shown = folder_name(&folder.display().to_string());
            out.push(Entry {
                label: tr(language, &format!("New {name} session in {shown}")),
                detail: folder.display().to_string(),
                hint: tr(language, "Action"),
                act: Act::NewSession {
                    agent: Some(agent.id.clone()),
                    cwd: Some(folder.clone()),
                },
                agent: Some(agent.id.clone()),
            });
        }
    }
    let act = tr(language, "Action");
    if s.mode == Mode::Session && s.selected.is_some() {
        out.push(action(
            tr(language, "Add to split view"),
            &keymap::label(Action::AddSplit),
            Act::Split,
        ));
        out.push(action(
            tr(language, "Toggle inspector"),
            &keymap::label(Action::ToggleInspector),
            Act::Inspector,
        ));
        out.push(action(
            tr(language, "Git"),
            &keymap::label(Action::ToggleGit),
            Act::Git,
        ));
        let dictating = s.voice.is_some();
        out.push(action(
            if dictating {
                tr(language, "Stop dictation")
            } else {
                tr(language, "Start dictation")
            },
            &keymap::label(Action::Dictation),
            Act::Dictation,
        ));
        let mut here = vec![Action::FocusComposer, Action::JumpLatest];
        if s.busy() {
            here.push(Action::Stop);
        }
        if s.catalog.is_some() {
            here.push(Action::ModelPicker);
        }
        if s.in_split() {
            here.push(Action::RemoveSplit);
            out.push(action(tr(language, "Close split view"), &act, Act::Unsplit));
        }
        for a in here {
            out.push(action(
                keymap::entry(a).what(language),
                &keymap::label(a),
                Act::Key(a),
            ));
        }
    }
    for (label, mode, key) in [
        (tr_ref(language, "Home"), Mode::Home, Action::Home),
        (tr_ref(language, "Inbox"), Mode::Inbox, Action::Inbox),
        (tr_ref(language, "Tasks"), Mode::Tasks, Action::Tasks),
    ] {
        out.push(action(
            format!("{} {label}", tr(language, "Go to")),
            &keymap::label(key),
            Act::Go(mode),
        ));
    }
    for a in [
        Action::Settings,
        Action::Shortcuts,
        Action::QuickOpen,
        Action::ToggleSidebar,
        Action::FocusSearch,
        Action::SearchMessages,
        Action::ToggleTerminal,
        Action::NewShell,
    ] {
        out.push(action(
            keymap::entry(a).what(language),
            &keymap::label(a),
            Act::Key(a),
        ));
    }
    for (label, tab) in super::settings::tabs(language) {
        out.push(action(
            tr(language, &format!("Settings: {label}")),
            &act,
            Act::Settings(tab),
        ));
    }
    out.push(action(
        if s.dark {
            tr(language, "Switch to light appearance")
        } else {
            tr(language, "Switch to dark appearance")
        },
        &act,
        Act::ToggleScheme,
    ));
    for theme in s.theme_library.iter() {
        out.push(Entry {
            label: tr(language, &format!("Theme: {}", theme.name)),
            detail: theme.author.clone().unwrap_or_default(),
            hint: if theme.id == s.cfg.appearance.theme_id {
                tr(language, "Current")
            } else {
                tr(language, "Theme")
            },
            act: Act::Theme(theme.id.clone()),
            agent: None,
        });
    }
    for plugin in s.plugins.all() {
        let pid = plugin.id().to_string();
        let ptitle = plugin.title(language);
        let pdesc = plugin.description(language).unwrap_or_default();
        let pcat = plugin.category().label(language).to_string();

        if plugin.supports_surface(crate::plugin::PluginSlot::Page) {
            out.push(Entry {
                label: format!("Open {ptitle} (Full Screen)"),
                detail: pdesc.clone(),
                hint: pcat.clone(),
                act: Act::PluginSurface {
                    plugin_id: pid.clone(),
                    surface: crate::plugin::PluginSlot::Page,
                },
                agent: None,
            });
        }
        if plugin.supports_surface(crate::plugin::PluginSlot::Tile) {
            out.push(Entry {
                label: format!("Open {ptitle} (Split Tile)"),
                detail: pdesc.clone(),
                hint: pcat.clone(),
                act: Act::PluginSurface {
                    plugin_id: pid.clone(),
                    surface: crate::plugin::PluginSlot::Tile,
                },
                agent: None,
            });
        }
        if plugin.supports_surface(crate::plugin::PluginSlot::Drawer) {
            out.push(Entry {
                label: format!("Open {ptitle} (Right Drawer)"),
                detail: pdesc.clone(),
                hint: pcat.clone(),
                act: Act::PluginSurface {
                    plugin_id: pid.clone(),
                    surface: crate::plugin::PluginSlot::Drawer,
                },
                agent: None,
            });
        }
        for cmd in plugin.commands() {
            out.push(Entry {
                label: format!("{ptitle}: {}", cmd.title),
                detail: cmd.id.to_string(),
                hint: cmd
                    .shortcut
                    .map(ToString::to_string)
                    .unwrap_or_else(|| pcat.clone()),
                act: Act::PluginCommand {
                    plugin_id: pid.clone(),
                    command_id: cmd.id.to_string(),
                },
                agent: None,
            });
        }
    }
    out
}

/// What the palette lists for `query`: recent commands first when empty,
/// else the best matches; each with its label's matched char indices.
pub fn results(s: &UiState, query: &str) -> Vec<(Entry, Vec<usize>)> {
    let all = entries(s);
    let query = query.trim();
    if query.is_empty() {
        let recent = &s.cfg.ui.recent_commands;
        let mut out: Vec<(Entry, Vec<usize>)> = recent
            .iter()
            .filter_map(|id| all.iter().find(|e| e.act.id() == *id).cloned())
            .map(|e| (e, Vec::new()))
            .collect();
        // Then recent sessions, then the actions without a target.
        for e in all.iter().filter(|e| {
            !recent.contains(&e.act.id()) && !matches!(&e.act, Act::NewSession { cwd: Some(_), .. })
        }) {
            out.push((e.clone(), Vec::new()));
            if out.len() >= 60 {
                break;
            }
        }
        return out;
    }
    let mut scored: Vec<(i32, usize, Entry, Vec<usize>)> = Vec::new();
    for (order, e) in all.into_iter().enumerate() {
        let on_label = fuzzy(query, &e.label);
        let on_detail = fuzzy(query, &format!("{} {}", e.label, e.detail));
        let (score, hits) = match (on_label, on_detail) {
            (Some((a, hits)), _) => (a + 10, hits),
            (None, Some((b, _))) => (b, Vec::new()),
            (None, None) => continue,
        };
        let recent_bonus = if s.cfg.ui.recent_commands.contains(&e.act.id()) {
            6
        } else {
            0
        };
        scored.push((score + recent_bonus, order, e, hits));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(60);
    scored.into_iter().map(|(_, _, e, h)| (e, h)).collect()
}

impl UiState {
    /// Ctrl+K (everything) or Ctrl+P (`sessions`): open, close, or switch
    /// an open palette to the other list.
    pub fn toggle_palette(&mut self, sessions: bool) {
        self.keys_sheet = None;
        self.palette = match &self.palette {
            Some(p) if p.sessions == sessions => None,
            _ => Some(Palette::new(sessions)),
        };
    }

    fn palette_move(&mut self, by: i32) {
        let query = match &self.palette {
            Some(p) => p.query.text.clone(),
            None => return,
        };
        let n = results(self, &query).len();
        if let Some(p) = self.palette.as_mut() {
            if n == 0 {
                p.selected = 0;
            } else {
                p.selected = (p.selected as i32 + by).rem_euclid(n as i32) as usize;
            }
        }
    }

    fn palette_run_selected(&mut self, tx: &mut Tx) {
        let Some(p) = self.palette.as_ref() else {
            return;
        };
        let (query, at) = (p.query.text.clone(), p.selected);
        if let Some((entry, _)) = results(self, &query).into_iter().nth(at) {
            self.run_act(entry.act, tx);
        }
    }

    pub fn run_act(&mut self, act: Act, tx: &mut Tx) {
        self.palette = None;
        let id = act.id();
        let mut recent = self.cfg.ui.recent_commands.clone();
        recent.retain(|r| *r != id);
        recent.insert(0, id);
        recent.truncate(RECENT);
        self.edit(ConfigEdit::RecentCommands(recent));
        match act {
            Act::Session(asid) => {
                let mods = tx.mods();
                if mods.ctrl || mods.cmd || mods.alt || mods.shift {
                    self.open_in_tile(&asid);
                } else {
                    self.open_session(&asid);
                }
            }
            Act::NewSession { agent, cwd } => {
                if let Some(agent) = agent {
                    self.start_agent = agent;
                }
                if let Some(cwd) = cwd {
                    self.start_cwd = TextEdit::new(cwd.display().to_string());
                    self.completions.clear();
                }
                self.go(Mode::Home);
            }
            Act::Go(mode) => self.go(mode),
            Act::Settings(tab) => {
                self.go(Mode::Settings);
                self.settings_tab = tab;
            }
            Act::Split => self.split(),
            Act::Unsplit => self.unsplit(),
            Act::Inspector => self.toggle_inspector(),
            Act::Git => self.toggle_git_inspector(),
            Act::Theme(id) => self.send(Command::ThemeApply(id, self.cfg.appearance.scheme)),
            Act::ToggleScheme => self.set_scheme(if self.dark {
                Scheme::Light
            } else {
                Scheme::Dark
            }),
            Act::Dictation => self.toggle_voice(),
            Act::Key(action) => {
                keymap::run(self, action, &rune::Key::Escape.into(), tx);
            }
            Act::PluginSurface { plugin_id, surface } => {
                self.switch_plugin_surface(&plugin_id, surface);
            }
            Act::PluginCommand {
                plugin_id,
                command_id,
            } => {
                if let Some(plugin) = self.plugins.get(&plugin_id) {
                    plugin.on_command(self, &command_id);
                }
            }
        }
    }
}

fn label_spans(k: &Kit, label: &str, hits: &[usize]) -> Vec<Span> {
    let mut spans = Vec::new();
    for (i, (at, c)) in label.char_indices().enumerate() {
        if hits.contains(&i) {
            spans.push(
                Span::new(at, at + c.len_utf8())
                    .color(k.c.text_accent)
                    .weight(650.0),
            );
        }
    }
    spans
}

/// Mount the palette above the window when it is open.
pub fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    let Some(p) = s.palette.as_ref() else {
        return;
    };
    let language = s.cfg.general.language;
    let look = s.look();
    let results = results(s, &p.query.text);
    let selected = p.selected.min(results.len().saturating_sub(1));

    let field = TextInput::new(&p.query)
        .placeholder(if p.sessions {
            tr_ref(language, "Go to a session by title or folder…")
        } else {
            tr_ref(language, "Jump to a session or run a command…")
        })
        .label(tr_ref(language, "Command palette"))
        .plain()
        .large()
        .on_edit(|s: &mut UiState| &mut s.palette.as_mut().expect("palette open").query)
        .on_change(|s: &mut UiState, _: &mut Tx| {
            if let Some(p) = s.palette.as_mut() {
                p.selected = 0;
            }
        })
        .on_key(
            |s: &mut UiState, ev: &rune::KeyEvent, tx: &mut Tx| match ev.key {
                rune::Key::Down => {
                    s.palette_move(1);
                    true
                }
                rune::Key::Up => {
                    s.palette_move(-1);
                    true
                }
                rune::Key::Enter => {
                    s.palette_run_selected(tx);
                    true
                }
                _ => false,
            },
        )
        .build(k)
        .key("palette-input")
        .w_full();

    let mut list = col().key("palette-list").w_full().gap(2.0).px(8.0).py(8.0);
    if results.is_empty() {
        list = list.child(
            text(tr(language, "Nothing matches"))
                .font(k.ty.ui)
                .color(k.c.text_faint)
                .px(12.0)
                .py(10.0),
        );
    }
    let first = selected.saturating_sub(SHOWN - 1);
    let recents = p.query.text.trim().is_empty() && !s.cfg.ui.recent_commands.is_empty();
    // `results` puts the recent commands that still exist first.
    let recent_count = if recents {
        results
            .iter()
            .take_while(|(e, _)| s.cfg.ui.recent_commands.contains(&e.act.id()))
            .count()
    } else {
        0
    };
    for (i, (entry, hits)) in results.iter().enumerate().skip(first).take(SHOWN) {
        if recents && i == 0 {
            list = list.child(section(k, tr(language, "Recent")));
        }
        if recents && i == recent_count && i > 0 {
            list = list.child(section(k, tr(language, "Sessions and actions")));
        }
        let on = i == selected;
        let mark = match &entry.agent {
            Some(agent) => super::agents::mark(agent, 16.0, s.dark),
            None => rune::icon(glyph(&entry.act), 16.0).color(k.c.icon_muted),
        };
        let mut words = col().grow(1.0).shrink().min_w(0.0).gap(1.0).child(
            rich(entry.label.clone(), label_spans(k, &entry.label, hits))
                .font(k.ty.ui)
                .color(if on { k.c.text_strong } else { k.c.text_base })
                .truncate(),
        );
        if !entry.detail.is_empty() {
            words = words.child(
                text(entry.detail.clone())
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .truncate(),
            );
        }
        let act = entry.act.clone();
        let mut line = row()
            .key(format!("pal-{}", entry.act.id()))
            .w_full()
            .gap(10.0)
            .center_y()
            .px(12.0)
            .py(7.0)
            .radius(k.radius.md)
            .child(mark)
            .child(words)
            .child(match entry.act.shortcut() {
                // A command's chord, as keycaps; else what the row is.
                Some(action) => rune_kit::Keybind::new(
                    &keymap::entry(action).keycaps_on(rune::KeyPlatform::current())[0],
                )
                .build(k),
                None => text(entry.hint.clone())
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            })
            .hover(k.c.hover)
            .role("option")
            .label(entry.label.clone())
            .on_press_tx(move |s: &mut UiState, tx: &mut Tx| s.run_act(act.clone(), tx));
        if on {
            line = line.fill(k.c.surface_hover);
            line = look.accent_rim(k, line, k.c.bg_accent);
        }
        list = list.child(line);
    }

    let foot = row()
        .key("palette-foot")
        .w_full()
        .gap(16.0)
        .px(18.0)
        .py(8.0)
        .child(hint(k, "↑ ↓", tr(language, "move")))
        .child(hint(k, "↵", tr(language, "run")))
        .child(hint(k, "Esc", tr(language, "close")))
        .child(el().grow(1.0))
        .child(
            text(format!("{} {}", results.len(), tr(language, "results")))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );

    let panel = col()
        .key("palette")
        .role("dialog")
        .label(tr(language, "Command palette"))
        .w_full()
        .max_w(640.0)
        .child(
            row()
                .key("palette-field")
                .w_full()
                .px(16.0)
                .py(10.0)
                .child(field),
        )
        .child(el().w_full().h(1.0).fill(k.c.border_muted))
        .child(list)
        .child(el().w_full().h(1.0).fill(k.c.border_muted))
        .child(foot);
    let mut panel = look.panel(k, panel, crate::theme::look::PANEL_RADIUS);
    for shadow in k.shadow_layers(&k.elevation.overlay) {
        panel = panel.box_shadow(shadow);
    }
    // A spring drop-in from just above; a plain fade under reduce motion.
    let panel = if s.cfg.appearance.reduced_motion {
        panel.transition(Transition::opacity())
    } else {
        panel
            .transition(
                Transition::opacity()
                    .combined(Transition::offset(0.0, -12.0))
                    .combined(Transition::scale(0.97)),
            )
            .motion(Spring::new(0.32, 0.16))
    }
    .modal(true, |s: &mut UiState| s.palette = None);

    layer.push(
        el().key("palette-scrim")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            // The kit's scrim is faint in some packs; the palette wants the
            // window to recede a little more.
            .fill(k.c.black.alpha(if s.dark { 0.38 } else { 0.18 }))
            .token_motion(k.motion.overlay)
            .a11y_hidden(),
    );
    layer.push(
        col()
            .key("palette-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .items_center()
            .pt(96.0)
            .px(24.0)
            .child(panel),
    );
}

fn glyph(act: &Act) -> &'static rune::Icon {
    match act {
        Act::Go(Mode::Inbox) => &super::icons::INBOX,
        Act::Go(Mode::Tasks) => &super::icons::TASK,
        Act::Dictation => &super::voice::MIC,
        Act::Git => &super::icons::BRANCH,
        Act::Settings(_)
        | Act::Inspector
        | Act::Split
        | Act::Unsplit
        | Act::Key(Action::Settings) => &super::icons::SETTINGS,
        Act::Theme(_) | Act::ToggleScheme => &rune_kit::glyphs::SPARKLE,
        Act::PluginSurface { .. } | Act::PluginCommand { .. } => &super::icons::GRID,
        _ => &rune_kit::glyphs::ARROW_RIGHT,
    }
}

fn section(k: &Kit, label: String) -> El {
    text(label)
        .font(k.ty.small_medium)
        .color(k.c.text_faint)
        .px(12.0)
        .pt(4.0)
        .pb(2.0)
}

fn hint(k: &Kit, key: &str, what: String) -> El {
    row()
        .gap(6.0)
        .center_y()
        .child(
            text(key.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
                .px(5.0)
                .py(1.0)
                .radius(4.0)
                .fill(k.c.layer_02),
        )
        .child(text(what).font(k.ty.small).color(k.c.text_faint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_in_order_and_prefers_word_starts() {
        assert!(fuzzy("xyz", "Go to Tasks").is_none());
        let (_, hits) = fuzzy("gtt", "Go to Tasks").unwrap();
        assert_eq!(hits, vec![0, 3, 6]);
        let starts = fuzzy("ns", "New session").unwrap().0;
        let middle = fuzzy("ns", "Insights").unwrap().0;
        assert!(starts > middle);
        assert_eq!(fuzzy("", "anything"), Some((0, Vec::new())));
    }
}
