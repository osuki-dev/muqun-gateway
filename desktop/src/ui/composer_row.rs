//! The session composer's control row: agent, model and effort, branch,
//! permission mode and auto-approve. It folds one step at a time to fit the
//! card it sits in (the card's own width, not the window's): labels become
//! icons, model and effort join into one chip, and last the least needed
//! controls move into a "…" menu, so the row stays one line beside the
//! send buttons and everything stays a click away.

use rune::{icon, row, text, Color, El, Icon};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Menu, MenuItem};

use super::composer::{
    catalog_models, effort_menu, mode_label, model_menu, switch_mode, switch_model,
};
use super::copy::tr_ref;
use super::labels::{bare_catalog_id, readable_catalog_id};
use super::parts::glyphs;
use super::{OpenMenu, UiState};
use crate::config::Language;
use crate::model::commands::Command;
use crate::model::compose::{BranchInfo, ComposeCommand};

// ---- glyphs (App set: trusted, locked, processor, more, approval-always) ----

static SHIELD: Icon = Icon {
    name: "cmp-shield",
    view: 24.0,
    body: r#"<path d="m12 3 8 3v7c0 4-8 8-8 8s-8-4-8-8V6l8-3Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static SHIELD_CHECK: Icon = Icon {
    name: "cmp-trusted",
    view: 24.0,
    body: r#"<path d="m12 3 8 3v7c0 4-8 8-8 8s-8-4-8-8V6l8-3Zm-4 9 3 3 5-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static LOCK: Icon = Icon {
    name: "cmp-locked",
    view: 24.0,
    body: r#"<rect x="5" y="10" width="14" height="11" rx="2.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M8 10V7a4 4 0 0 1 8 0v3m-4 5v2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static MODEL: Icon = Icon {
    name: "cmp-processor",
    view: 24.0,
    body: r#"<rect x="6" y="6" width="12" height="12" rx="3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M9 3v3m6-3v3M9 18v3m6-3v3M3 9h3m-3 6h3m12-6h3m-3 6h3M10 10h4v4h-4v-4Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static AUTO_APPROVE: Icon = Icon {
    name: "cmp-approval-always",
    view: 24.0,
    body: r#"<path d="M12 3 4 6v6c0 5 8 9 8 9s8-4 8-9V6l-8-3Zm-4 9 3 3 5-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

/// A permission mode's glyph, and whether it is the risky one (bypass:
/// everything runs unasked) that takes the warning tone.
fn mode_glyph(id: &str) -> (&'static Icon, bool) {
    let id = bare_catalog_id(id).to_ascii_lowercase();
    if ["bypass", "yolo", "danger", "full-access", "full_access"]
        .iter()
        .any(|w| id.contains(w))
    {
        (&glyphs::TRUST_WARNING, true)
    } else if id.contains("plan") {
        (&glyphs::TASK_LIST, false)
    } else if id.contains("dontask") || id.contains("read") || id.contains("deny") {
        (&LOCK, false)
    } else if id.contains("accept") || id.contains("auto") {
        (&SHIELD_CHECK, false)
    } else {
        (&SHIELD, false)
    }
}

// ---- metrics ------------------------------------------------------------------------

/// A pill (`ControlSize::Small` ghost/neutral button): 10 pt each side + border/rounding headroom.
const PILL_PAD: f32 = 22.0;
/// `composer::pill` (`model_menu` / `effort_menu`): 12 pt each side + 1 pt border each side.
const MENU_PILL_PAD: f32 = 26.0;
/// Width of `ActivityMark::Compact` (12 pt) plus its 5 pt gap inside `agent_chip`.
const BUSY_MARK_W: f32 = 17.0;
/// Compact controls keep enough room around their glyph.
const ICON_PILL: f32 = 32.0;
const GLYPH: f32 = 14.0;
/// Between pills in the row.
const ROW_GAP: f32 = 8.0;
/// Between the composer's action buttons.
pub const ACTION_GAP: f32 = 6.0;
/// The card's own side padding (12 pt each side).
pub const CARD_PAD: f32 = 24.0;

fn text_w(s: &str, style: rune::TextStyle) -> f32 {
    rune::text::measure(s, &[], style, None).0
}

// ---- what the row shows -------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModelLook {
    /// Model and effort, two chips.
    Split,
    /// One chip: "Opus 5.5 · Medium".
    Joined,
    /// The model's name; effort is in its menu.
    Name,
    /// The name's first word ("Opus"); the rest in the tooltip and menu.
    Short,
    /// The model glyph; name and effort in the tooltip and menu.
    Icon,
}

/// One fold of the row, from [`FOLDS`]: each step folds one more thing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fold {
    agent_icon: bool,
    model: ModelLook,
    mode_icon: bool,
    branch_icon: bool,
    more_branch: bool,
    more_mode: bool,
    more_agent: bool,
}

const FULL: Fold = Fold {
    agent_icon: false,
    model: ModelLook::Split,
    mode_icon: false,
    branch_icon: false,
    more_branch: false,
    more_mode: false,
    more_agent: false,
};

/// The steps, in the order the row gives them up: words before meaning;
/// the permission control (a bypass or auto-approve is worth seeing) is the
/// last to leave, after the agent, which the pane's header names anyway.
const FOLDS: [fn(&mut Fold); 10] = [
    |f| f.agent_icon = true,
    |f| f.model = ModelLook::Joined,
    |f| f.mode_icon = true,
    |f| f.branch_icon = true,
    |f| f.model = ModelLook::Name,
    |f| f.model = ModelLook::Short,
    |f| f.more_branch = true,
    |f| f.model = ModelLook::Icon,
    |f| f.more_agent = true,
    |f| f.more_mode = true,
];

fn fold_at(level: usize) -> Fold {
    let mut f = FULL;
    for step in FOLDS.iter().take(level) {
        step(&mut f);
    }
    f
}

struct ModelChip {
    name: String,
    /// The effort's label when the model has effort levels.
    effort: Option<String>,
    variant: Option<String>,
    variants: Vec<String>,
    models: Vec<(String, String, String)>,
    current: Option<muqun_gateway::agents::ModelRef>,
}

struct ModeChoice {
    id: String,
    label: String,
    description: Option<String>,
}

struct PermissionSummary {
    glyph: &'static Icon,
    label: String,
    warn: bool,
    help: String,
}

/// The open session's controls, read once per frame.
pub struct Controls {
    language: Language,
    dark: bool,
    agent_id: String,
    agent_name: String,
    model: Option<ModelChip>,
    branch: Option<(String, BranchInfo)>,
    modes: Vec<ModeChoice>,
    mode: Option<String>,
    asid: muqun_gateway::agents::AgentSessionId,
    auto_on: bool,
    auto_visible: bool,
    auto_applies: String,
    busy: bool,
}

/// Show a separate approval toggle section only when discovery offers a live
/// session control that is not already represented by the session's mode menu.
fn standalone_approval(feature: Option<&serde_json::Value>, modes: &[ModeChoice]) -> bool {
    let Some(feature) = feature.filter(|f| f["scope"] == "session") else {
        return false;
    };
    let Some(native) = feature["native"].as_str().filter(|s| !s.is_empty()) else {
        return false;
    };
    !modes.iter().any(|choice| {
        let id = bare_catalog_id(&choice.id);
        id == native
            || feature["modeIds"]
                .as_array()
                .is_some_and(|aliases| aliases.iter().any(|alias| alias.as_str() == Some(id)))
    })
}

impl Controls {
    pub fn for_session(
        s: &UiState,
        asid: &str,
        catalog: Option<&crate::hub::CatalogView>,
    ) -> Option<Controls> {
        let info = s.session_by_id(asid)?;
        let models = catalog.map(catalog_models).unwrap_or_default();
        let default_model = catalog.and_then(|c| c.catalog.defaults.model.as_ref());
        let model = match info.model.as_ref().or(default_model) {
            Some(m) => {
                let selected = catalog.and_then(|v| super::labels::catalog_model_info(v, m));
                let variants: Vec<String> = selected
                    .and_then(|i| i.variants.as_ref())
                    .map(|v| v.iter().map(|v| v.id.clone()).collect())
                    .unwrap_or_default();
                Some(ModelChip {
                    name: super::labels::format_model_name(
                        m,
                        "Model",
                        selected.map(|i| i.name.as_str()),
                    ),
                    effort: (!variants.is_empty()).then(|| effort_label(m.variant.as_deref())),
                    variant: m.variant.clone(),
                    variants,
                    current: Some(m.clone()),
                    models,
                })
            }
            None if !models.is_empty() => Some(ModelChip {
                name: "Model".into(),
                effort: None,
                variant: None,
                variants: Vec::new(),
                current: None,
                models,
            }),
            None => None,
        };
        let branch = info.directory.as_ref().and_then(|cwd| {
            s.branches
                .get(cwd)
                .filter(|b| b.current.is_some())
                .map(|b| (cwd.clone(), b.clone()))
        });
        let modes: Vec<ModeChoice> = catalog
            .map(|c| {
                c.catalog
                    .modes
                    .iter()
                    .filter(|m| !m.hidden)
                    .map(|m| ModeChoice {
                        id: m.id.clone(),
                        label: mode_label(&m.id, Some(m.name.as_str())),
                        description: m
                            .description
                            .as_deref()
                            .map(str::trim)
                            .filter(|d| !d.is_empty())
                            .map(str::to_owned),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let auto_feature = s
            .agents
            .iter()
            .find(|a| a.id == info.agent_id)
            .and_then(|a| a.features.extra.get("autoApprove"));
        let auto_visible = standalone_approval(auto_feature, &modes);
        let auto_applies = auto_feature
            .and_then(|f| f["applies"].as_str())
            .unwrap_or("")
            .to_owned();
        Some(Controls {
            language: s.cfg.general.language,
            dark: s.dark,
            agent_id: info.agent_id.clone(),
            agent_name: s.agent_name(&info.agent_id),
            model,
            branch,
            modes,
            mode: info.mode.clone(),
            asid: info.asid.clone(),
            auto_visible,
            auto_applies,
            auto_on: s.auto_approve.get(&info.asid.0).copied().unwrap_or(false),
            busy: s.session_busy(asid),
        })
    }

    fn tr(&self, en: &'static str) -> &'static str {
        tr_ref(self.language, en)
    }

    /// The current mode's id (as the catalogue spells it) and label.
    fn current_mode(&self) -> Option<(String, String)> {
        let mode = self.mode.as_ref()?;
        Some(
            self.modes
                .iter()
                .find(|m| m.id == *mode || bare_catalog_id(&m.id) == mode.as_str())
                .map(|m| (m.id.clone(), m.label.clone()))
                .unwrap_or_else(|| (mode.clone(), mode_label(mode, None))),
        )
    }

    fn has_permission_control(&self) -> bool {
        self.auto_visible || self.modes.len() > 1 || self.mode.is_some()
    }

    fn permission_interactive(&self) -> bool {
        self.auto_visible || self.modes.len() > 1
    }

    fn permission_summary(&self) -> PermissionSummary {
        if self.auto_visible {
            let mode_prefix = (self.modes.len() > 1)
                .then(|| self.current_mode().map(|(_, label)| label))
                .flatten();
            if self.auto_on {
                let help = match &mode_prefix {
                    Some(mode) => format!("{mode} · {}", auto_help(self)),
                    None => auto_help(self),
                };
                return PermissionSummary {
                    glyph: &AUTO_APPROVE,
                    label: self.tr("Auto-approve").to_string(),
                    warn: true,
                    help,
                };
            }
            if self.modes.len() < 2 {
                return PermissionSummary {
                    glyph: &SHIELD,
                    label: self.tr("Ask").to_string(),
                    warn: false,
                    help: auto_help(self),
                };
            }
        }
        let (id, label) = self
            .current_mode()
            .unwrap_or_else(|| (String::new(), self.tr("Mode").to_string()));
        let (glyph, warn) = mode_glyph(&id);
        let help = if self.auto_visible {
            format!(
                "{} {label} · {}",
                self.tr("Permission mode:"),
                auto_help(self)
            )
        } else {
            format!("{} {label}", self.tr("Permission mode:"))
        };
        PermissionSummary {
            glyph,
            label,
            warn,
            help,
        }
    }

    fn branch_text(&self, compact: bool) -> Option<&str> {
        let (_, info) = self.branch.as_ref()?;
        let current = info.current.as_deref().unwrap_or("Git");
        if compact && current.len() > 3 {
            Some("Git")
        } else {
            Some(current)
        }
    }

    /// Whether anything goes into the "…" menu at `f`.
    fn has_more(&self, f: Fold) -> bool {
        (f.more_branch && self.branch.is_some())
            || (f.more_mode && self.has_permission_control())
            || f.more_agent
    }

    /// The row's width at fold `f`, as it will lay out.
    fn width(&self, k: &Kit, f: Fold) -> f32 {
        let btn_font = k.ty.small_medium;
        let mut items: Vec<f32> = Vec::new();
        if !f.more_agent {
            items.push(if f.agent_icon {
                ICON_PILL
            } else {
                let busy_w = if self.busy { BUSY_MARK_W } else { 0.0 };
                7.0 + super::composer::AGENT_ICON
                    + 5.0
                    + text_w(&self.agent_name, k.ty.small_medium)
                    + busy_w
                    + 7.0
            });
        }
        if let Some(m) = &self.model {
            let reload_only = m.models.is_empty() && m.variants.is_empty();
            match f.model {
                ModelLook::Split => {
                    if m.models.is_empty() {
                        items.push(text_w(&format!("{} ↻", m.name), btn_font) + PILL_PAD);
                    } else {
                        items.push(text_w(&m.name, btn_font) + MENU_PILL_PAD);
                    }
                    if let Some(e) = &m.effort {
                        items.push(text_w(e, btn_font) + MENU_PILL_PAD);
                    }
                }
                ModelLook::Joined => {
                    if reload_only {
                        items.push(text_w(&reload_label(m, f.model), btn_font) + PILL_PAD);
                    } else {
                        items.push(text_w(&joined(m), btn_font) + PILL_PAD);
                    }
                }
                ModelLook::Name => {
                    if reload_only {
                        items.push(text_w(&reload_label(m, f.model), btn_font) + PILL_PAD);
                    } else {
                        items.push(text_w(&m.name, btn_font) + PILL_PAD);
                    }
                }
                ModelLook::Short => {
                    if reload_only {
                        items.push(text_w(&reload_label(m, f.model), btn_font) + PILL_PAD);
                    } else {
                        items.push(text_w(short_name(&m.name), btn_font) + PILL_PAD);
                    }
                }
                ModelLook::Icon => {
                    if reload_only {
                        items.push(
                            (text_w(&reload_label(m, f.model), btn_font) + PILL_PAD).max(ICON_PILL),
                        );
                    } else {
                        items.push(ICON_PILL);
                    }
                }
            }
        }
        if !f.more_branch {
            if let Some(label) = self.branch_text(f.branch_icon) {
                items.push(text_w(label, btn_font) + PILL_PAD);
            }
        }
        if !f.more_mode && self.has_permission_control() {
            if f.mode_icon {
                items.push(ICON_PILL);
            } else {
                let summary = self.permission_summary();
                items.push(GLYPH + 4.0 + text_w(&summary.label, btn_font) + PILL_PAD);
            }
        }
        if self.has_more(f) {
            items.push(ICON_PILL);
        }
        let gaps = items.len().saturating_sub(1) as f32 * ROW_GAP;
        items.iter().sum::<f32>() + gaps
    }

    /// The richest fold that fits `room` points, or `None` when even the
    /// narrowest does not.
    fn fit(&self, k: &Kit, room: f32) -> Option<Fold> {
        (0..=FOLDS.len())
            .map(fold_at)
            .find(|f| self.width(k, *f) <= room)
    }
}

fn effort_label(variant: Option<&str>) -> String {
    variant
        .map(readable_catalog_id)
        .unwrap_or_else(|| "Default".into())
}

/// A model name's first word: "Opus 5.5" → "Opus".
fn short_name(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

fn joined(m: &ModelChip) -> String {
    match &m.effort {
        Some(e) => format!("{} · {e}", m.name),
        None => m.name.clone(),
    }
}

fn reload_label(m: &ModelChip, look: ModelLook) -> String {
    match look {
        ModelLook::Split | ModelLook::Name => format!("{} ↻", m.name),
        ModelLook::Joined => format!("{} ↻", joined(m)),
        ModelLook::Short => format!("{} ↻", short_name(&m.name)),
        ModelLook::Icon => "↻".to_string(),
    }
}

// ---- pieces -------------------------------------------------------------------------

/// What a pill shows.
enum Face {
    Text(String),
    /// A glyph (in `tone`, else the pill's ink) and a word.
    Labelled(&'static Icon, Option<Color>, String),
    /// A glyph alone: the pill's label and tooltip say what it is.
    Glyph(&'static Icon, Option<Color>),
}

/// A ghost pill that opens `menu` (toggling it). `label` is what assistive
/// technology reads; `help` the tooltip.
fn pill_button(
    k: &Kit,
    key: &str,
    face: Face,
    label: String,
    help: Option<String>,
    open: bool,
    menu: OpenMenu,
) -> El {
    let square = matches!(face, Face::Glyph(..));
    let mut b = Button::new(label)
        .variant(ButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .expanded(open)
        .on_press(move |s: &mut UiState| s.toggle_composer_menu(menu.clone()));
    if let Some(help) = help {
        b = b.help(help);
    }
    b = match face {
        Face::Text(t) => b.label_with(move |font, ink| text(t).font(font).color(ink)),
        Face::Labelled(glyph, tone, t) => b.label_with(move |font, ink| {
            let ink = tone.unwrap_or(ink);
            row()
                .gap(4.0)
                .center_y()
                .child(icon(glyph, GLYPH).color(ink))
                .child(text(t).font(font).color(ink))
        }),
        Face::Glyph(glyph, tone) => {
            b.label_with(move |_, ink| icon(glyph, GLYPH).color(tone.unwrap_or(ink)))
        }
    };
    let mut e = b.build(k).key(key.to_string());
    if square {
        e = e.w(ICON_PILL).min_w(ICON_PILL).px(0.0);
    }
    let [c0, c1, _] = crate::theme::look::highlight_colors_for(k, k.dark);
    let mut e = e.h(32.0).radius(k.radius.lg).border(0.0, c0.alpha(0.0));
    if open {
        e = e.linear_gradient(
            std::f32::consts::FRAC_PI_2,
            [(0.0, c0.alpha(0.20)), (1.0, c1.alpha(0.14))],
        );
    } else {
        e = e.fill(k.c.layer_01.alpha(0.38));
    }
    e
}

fn agent_chip(c: &Controls, k: &Kit, f: Fold) -> El {
    let mark = super::agents::mark(&c.agent_id, super::composer::AGENT_ICON, c.dark).a11y_hidden();
    let mut chip = row()
        .key("agent")
        .role("image")
        .label(c.agent_name.clone())
        .center_y()
        .h(32.0)
        .radius(k.radius.lg)
        .fill(k.c.layer_01.alpha(0.38))
        .child(mark);
    if f.agent_icon {
        return chip
            .w(ICON_PILL)
            .min_w(ICON_PILL)
            .radius(k.radius.lg)
            .center()
            .help(c.agent_name.clone());
    }
    chip = chip.gap(5.0).px(7.0).child(
        text(c.agent_name.clone())
            .font(k.ty.small_medium)
            .color(k.c.text_muted)
            .a11y_hidden(),
    );
    if c.busy {
        chip = chip.child(crate::ui::art::ActivityMark::Compact.build(k));
    }
    chip
}

fn reload_model(k: &Kit, c: &Controls, label: String) -> El {
    Button::new(label)
        .variant(ButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .help(c.tr("Reload model list"))
        .on_press(|s: &mut UiState| {
            if let Some(info) = s.current().cloned() {
                s.session_opened(&info);
            }
        })
        .build(k)
        .key("model-reload")
        .h(32.0)
        .radius(k.radius.lg)
}

fn model_chips(
    s: &UiState,
    c: &Controls,
    catalog: Option<&crate::hub::CatalogView>,
    k: &Kit,
    layer: &mut Vec<El>,
    f: Fold,
    line: El,
) -> El {
    let Some(m) = &c.model else { return line };
    let open = s.menu == Some(OpenMenu::Model);
    if f.model == ModelLook::Split {
        let mut line = line;
        line = line.child(if m.models.is_empty() {
            reload_model(k, c, reload_label(m, f.model))
        } else {
            model_menu(
                s,
                k,
                layer,
                OpenMenu::Model,
                &c.asid.0,
                super::Slot::Session,
                &c.agent_id,
                open,
                m.name.clone(),
                catalog,
                &m.models,
                m.current.as_ref(),
                switch_model,
            )
        });
        if !m.variants.is_empty() {
            line = line.child(effort_menu(
                k,
                layer,
                &c.asid.0,
                super::Slot::Session,
                s.menu == Some(OpenMenu::Effort),
                m.variant.as_deref(),
                &m.variants,
            ));
        }
        return line;
    }
    let whole = joined(m);
    let help = format!("{} {whole}", c.tr("Model:"));
    let face = match f.model {
        ModelLook::Joined => Face::Text(whole.clone()),
        ModelLook::Name => Face::Text(m.name.clone()),
        ModelLook::Short => Face::Text(short_name(&m.name).to_string()),
        _ => Face::Glyph(&MODEL, None),
    };
    if m.models.is_empty() && m.variants.is_empty() {
        return line.child(reload_model(k, c, reload_label(m, f.model)));
    }
    let trigger = pill_button(
        k,
        "model-pill",
        face,
        help.clone(),
        Some(help),
        open,
        OpenMenu::Model,
    );
    line.child(super::model_picker::popup(
        s,
        k,
        layer,
        &format!("model-{}", c.asid.0),
        trigger,
        open,
        super::Slot::Session,
        &c.agent_id,
        catalog,
        &m.models,
        m.current.as_ref(),
        switch_model,
    ))
}

fn mode_items(menu: Menu, c: &Controls, k: &Kit) -> Menu {
    let current = c.current_mode().map(|(id, _)| id);
    let mut menu = menu;
    for choice in &c.modes {
        let (glyph, warn) = mode_glyph(&choice.id);
        let pick = choice.id.clone();
        let mut item = MenuItem::new(choice.label.clone())
            .lead(icon(glyph, GLYPH).color(if warn {
                k.c.warning.icon
            } else {
                k.c.icon_muted
            }))
            .checked(current.as_deref() == Some(choice.id.as_str()))
            .on_select(move |s: &mut UiState| switch_mode(s, pick.clone()));
        if let Some(desc) = &choice.description {
            item = item.description(desc.clone());
        } else if warn {
            item = item.description(c.tr("Nothing asks first"));
        }
        menu = menu.item(item);
    }
    menu
}

fn auto_timing(c: &Controls) -> &'static str {
    match c.auto_applies.as_str() {
        "next_turn" => c.tr("Applies to the next turn"),
        "restart_when_idle" => c.tr("Restarts this session when idle"),
        _ => c.tr("Applies to this session"),
    }
}

fn auto_items(menu: Menu, c: &Controls, k: &Kit) -> Menu {
    let (on, asid) = (c.auto_on, c.asid.clone());
    let asid_off = asid.clone();
    let restart_blocked = c.busy && c.auto_applies == "restart_when_idle";
    let ask_desc = if restart_blocked {
        c.tr("Wait for the current turn to finish").to_string()
    } else {
        c.tr("Ask before running tools or editing files")
            .to_string()
    };
    let auto_desc = if restart_blocked {
        format!(
            "{} · {}",
            c.tr("Wait for the current turn to finish"),
            auto_timing(c)
        )
    } else {
        format!("{} · {}", c.tr("Nothing asks first"), auto_timing(c))
    };
    menu.item(
        MenuItem::new(c.tr("Ask before acting"))
            .lead(icon(&SHIELD, GLYPH).color(k.c.icon_muted))
            .description(ask_desc)
            .checked(!on)
            .disabled(restart_blocked)
            .on_select(move |s: &mut UiState| {
                s.menu = None;
                if on {
                    set_auto(s, &asid_off, false);
                }
            }),
    )
    .item(
        MenuItem::new(c.tr("Auto-approve"))
            .lead(icon(&AUTO_APPROVE, GLYPH).color(k.c.warning.icon))
            .description(auto_desc)
            .checked(on)
            .disabled(restart_blocked)
            .on_select(move |s: &mut UiState| {
                s.menu = None;
                if !on {
                    set_auto(s, &asid, true);
                }
            }),
    )
}

fn permission_items(menu: Menu, c: &Controls, k: &Kit) -> Menu {
    let mut menu = menu;
    let has_modes = c.modes.len() > 1;
    if has_modes {
        menu = mode_items(menu.group_label(c.tr("Permission mode")), c, k);
        if c.auto_visible {
            menu = menu.separator();
        }
    }
    if c.auto_visible {
        menu = auto_items(menu.group_label(c.tr("Permissions")), c, k);
    }
    menu
}

fn permission_control(
    s: &UiState,
    c: &Controls,
    k: &Kit,
    layer: &mut Vec<El>,
    f: Fold,
) -> Option<El> {
    if !c.has_permission_control() {
        return None;
    }
    let summary = c.permission_summary();
    let icon_tone = summary.warn.then_some(k.c.warning.icon);
    let text_tone = summary.warn.then_some(k.c.warning.fg);
    if !c.permission_interactive() {
        // Nothing to switch to: show the read-only mode clearly.
        let chip = row()
            .key("mode")
            .role("image")
            .label(summary.help.clone())
            .help(summary.help)
            .h(32.0)
            .center_y()
            .child(
                icon(summary.glyph, GLYPH)
                    .color(icon_tone.unwrap_or(k.c.icon_muted))
                    .a11y_hidden(),
            );
        return Some(if f.mode_icon {
            chip.w(ICON_PILL).center()
        } else {
            chip.gap(4.0).px(6.0).child(
                text(summary.label)
                    .font(k.ty.small)
                    .color(text_tone.unwrap_or(k.c.text_muted))
                    .a11y_hidden(),
            )
        });
    }
    let open = s.menu == Some(OpenMenu::Mode);
    let face = if f.mode_icon {
        Face::Glyph(summary.glyph, icon_tone)
    } else {
        Face::Labelled(summary.glyph, text_tone.or(icon_tone), summary.label)
    };
    let mut trigger = pill_button(
        k,
        "mode-pill",
        face,
        summary.help.clone(),
        Some(summary.help),
        open,
        OpenMenu::Mode,
    );
    if summary.warn {
        let [c0, _, c2] = crate::theme::look::highlight_colors_for(k, k.dark);
        let a = if open { 0.24 } else { 0.16 };
        let b = if open { 0.16 } else { 0.10 };
        trigger = trigger
            .linear_gradient(
                std::f32::consts::FRAC_PI_2,
                [(0.0, c0.alpha(a)), (1.0, c2.alpha(b))],
            )
            .border(0.0, c0.alpha(0.0));
    }
    let menu = Menu::new(format!("mode-{}", c.asid.0))
        .placement(rune_kit::Placement::TopStart)
        .trigger(trigger)
        .open(open)
        .min_width(240.0)
        .on_close(|s: &mut UiState| s.menu = None);
    Some(permission_items(menu, c, k).build(k, layer))
}

fn set_auto(s: &mut UiState, asid: &muqun_gateway::agents::AgentSessionId, on: bool) {
    s.send(Command::Compose(ComposeCommand::SetAutoApprove {
        asid: asid.clone(),
        enabled: on,
    }));
}

fn auto_help(c: &Controls) -> String {
    format!(
        "{} · {} · {} · {}",
        c.agent_name,
        c.tr("Auto-approve"),
        if c.auto_on {
            c.tr("Enabled")
        } else {
            c.tr("Disabled")
        },
        auto_timing(c)
    )
}

fn branch_control(
    _s: &UiState,
    c: &Controls,
    k: &Kit,
    _layer: &mut Vec<El>,
    f: Fold,
) -> Option<El> {
    let label = c.branch_text(f.branch_icon)?;
    Some(
        Button::new(label.to_string())
            .variant(ButtonVariant::Neutral)
            .control_size(ControlSize::Small)
            .on_press(|s: &mut UiState| s.toggle_git_panel())
            .build(k)
            .key("branch")
            .h(32.0)
            .radius(k.radius.lg),
    )
}

/// The "…" menu: whatever the fold moved out of the row.
fn more_menu(s: &UiState, c: &Controls, k: &Kit, layer: &mut Vec<El>, f: Fold) -> El {
    let open = s.menu == Some(OpenMenu::ComposerMore);
    let label = c.tr("More controls");
    let trigger = pill_button(
        k,
        "composer-more-pill",
        Face::Glyph(&super::icons::MORE, None),
        label.to_string(),
        Some(label.to_string()),
        open,
        OpenMenu::ComposerMore,
    );
    let mut menu = Menu::new(format!("composer-more-{}", c.asid.0))
        .placement(rune_kit::Placement::TopStart)
        .trigger(trigger)
        .open(open)
        .min_width(220.0)
        .on_close(|s: &mut UiState| s.menu = None);
    let mut first = true;
    let mut section = |menu: Menu| {
        let menu = if first { menu } else { menu.separator() };
        first = false;
        menu
    };
    if f.more_agent {
        menu = section(menu).item(
            MenuItem::new(c.agent_name.clone())
                .lead(super::agents::mark(
                    &c.agent_id,
                    super::composer::AGENT_ICON,
                    c.dark,
                ))
                .disabled(true),
        );
    }
    if f.more_mode && c.permission_interactive() {
        menu = permission_items(section(menu), c, k);
    } else if f.more_mode {
        if let Some((id, label)) = c.current_mode() {
            let (glyph, warn) = mode_glyph(&id);
            menu = section(menu).item(
                MenuItem::new(label)
                    .lead(icon(glyph, GLYPH).color(if warn {
                        k.c.warning.icon
                    } else {
                        k.c.icon_muted
                    }))
                    .disabled(true),
            );
        }
    }
    if let (true, Some((_, info))) = (f.more_branch, c.branch.as_ref()) {
        menu = section(menu).item(
            MenuItem::new("Git")
                .lead(icon(&super::icons::BRANCH, GLYPH).color(k.c.icon_muted))
                .description(info.current.clone().unwrap_or_default())
                .on_select(|s: &mut UiState| {
                    s.menu = None;
                    s.toggle_git_panel();
                }),
        );
    }
    menu.build(k, layer)
}

fn build(
    s: &UiState,
    c: &Controls,
    catalog: Option<&crate::hub::CatalogView>,
    k: &Kit,
    layer: &mut Vec<El>,
    f: Fold,
) -> El {
    let mut line = row()
        .key("pills")
        .gap(ROW_GAP)
        .center_y()
        .min_w(0.0)
        .shrink()
        .clip_soft(0.0);
    if !f.more_agent {
        line = line.child(agent_chip(c, k, f));
    }
    line = model_chips(s, c, catalog, k, layer, f, line);
    if !f.more_branch {
        if let Some(b) = branch_control(s, c, k, layer, f) {
            line = line.child(b);
        }
    }
    if !f.more_mode {
        if let Some(m) = permission_control(s, c, k, layer, f) {
            line = line.child(m);
        }
    }
    if c.has_more(f) {
        line = line.child(more_menu(s, c, k, layer, f));
    }
    line
}

/// The row on a line of its own, `room` points wide.
pub fn own_line(
    s: &UiState,
    c: &Controls,
    catalog: Option<&crate::hub::CatalogView>,
    k: &Kit,
    layer: &mut Vec<El>,
    room: f32,
) -> El {
    let f = c.fit(k, room).unwrap_or_else(|| fold_at(FOLDS.len()));
    build(s, c, catalog, k, layer, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(id: &str, label: &str) -> ModeChoice {
        ModeChoice {
            id: id.into(),
            label: label.into(),
            description: None,
        }
    }

    #[test]
    fn approval_control_requires_capability_and_deduplicates_offered_modes() {
        use serde_json::json;
        let modes = vec![choice("bypassPermissions[]", "Bypass")];
        let feature =
            json!({"scope":"session", "native":"yolo", "modeIds":["yolo","bypassPermissions"]});
        assert!(!standalone_approval(Some(&feature), &modes));
        assert!(standalone_approval(
            Some(&feature),
            &[choice("plan", "Plan")]
        ));
        assert!(!standalone_approval(None, &[]));
        assert!(!standalone_approval(
            Some(&json!({"scope":"launch", "native":"flag"})),
            &[]
        ));
        assert!(!standalone_approval(Some(&json!({"scope":"session"})), &[]));
        // Older gateways have no alias list; their exact native mode still deduplicates.
        assert!(!standalone_approval(
            Some(&json!({"scope":"session", "native":"bypassPermissions"})),
            &modes
        ));
    }

    fn claude() -> Controls {
        let efforts = vec!["low".into(), "medium".into(), "high".into()];
        Controls {
            language: Language::En,
            dark: false,
            agent_id: "claude".into(),
            agent_name: "Claude Code".into(),
            model: Some(ModelChip {
                name: "Opus 5.5".into(),
                effort: Some("Medium".into()),
                variant: Some("medium".into()),
                variants: efforts,
                models: vec![("anthropic".into(), "opus-5.5".into(), "Opus 5.5".into())],
                current: Some(muqun_gateway::agents::ModelRef {
                    provider_id: "anthropic".into(),
                    model_id: "opus-5.5".into(),
                    variant: Some("medium".into()),
                }),
            }),
            branch: Some((
                "/w".into(),
                BranchInfo {
                    current: Some("main".into()),
                    local: vec!["main".into(), "next".into()],
                    ..Default::default()
                },
            )),
            modes: [
                "default",
                "acceptEdits",
                "plan",
                "dontAsk",
                "bypassPermissions",
            ]
            .iter()
            .map(|m| choice(m, &readable_catalog_id(m)))
            .collect(),
            mode: Some("bypassPermissions".into()),
            asid: muqun_gateway::agents::AgentSessionId("s".into()),
            auto_on: true,
            auto_visible: false,
            auto_applies: "immediate".into(),
            busy: false,
        }
    }

    fn antigravity(auto_on: bool) -> Controls {
        Controls {
            language: Language::En,
            dark: false,
            agent_id: "antigravity".into(),
            agent_name: "Antigravity".into(),
            model: None,
            branch: None,
            modes: vec![],
            mode: Some("default".into()),
            asid: muqun_gateway::agents::AgentSessionId("ag".into()),
            auto_on,
            auto_visible: true,
            auto_applies: "restart_when_idle".into(),
            busy: false,
        }
    }

    /// Less room never shows more: each fold is no wider than the one
    /// before, so the row only ever folds further as a pane narrows.
    #[test]
    fn narrower_room_never_unfolds() {
        let k = Kit::neutral(false);
        for c in [claude(), antigravity(false), antigravity(true)] {
            let widths: Vec<f32> = (0..=FOLDS.len()).map(|l| c.width(&k, fold_at(l))).collect();
            assert!(widths.windows(2).all(|w| w[1] <= w[0]), "{widths:?}");
        }
    }

    #[test]
    fn single_mode_auto_approve_agent_shows_clear_unified_permission_label() {
        let off = antigravity(false).permission_summary();
        assert_eq!(off.label, "Ask");
        assert!(!off.warn);

        let on = antigravity(true).permission_summary();
        assert_eq!(on.label, "Auto-approve");
        assert!(on.warn);
        assert!(on.help.contains("Restarts this session when idle"));
    }
}
