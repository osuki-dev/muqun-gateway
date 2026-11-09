//! The session composer's control row: agent, model and effort, branch,
//! permission mode and auto-approve. It folds one step at a time to fit the
//! card it sits in (the card's own width, not the window's): labels become
//! icons, model and effort join into one chip, and last the least needed
//! controls move into a "…" menu, so the row stays one line beside the
//! send buttons and everything stays a click away.

use rune::{icon, row, text, Color, El, Icon, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, Kit, Menu, MenuItem, SegmentedControl,
};

use super::composer::{
    branch_items, catalog_models, effort_menu, mode_label, model_menu, switch_effort, switch_mode,
    switch_model,
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

static MORE: Icon = Icon {
    name: "cmp-more",
    view: 24.0,
    body: r#"<circle cx="5" cy="12" r="1" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><circle cx="12" cy="12" r="1" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><circle cx="19" cy="12" r="1" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static AUTO_APPROVE: Icon = Icon {
    name: "cmp-approval-always",
    view: 24.0,
    body: r#"<path d="m3 12 4 4L17 6m-5 11 3 3 7-9" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

static BRANCH: Icon = Icon {
    name: "cmp-branch",
    view: 24.0,
    body: r#"<circle cx="6" cy="5" r="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><circle cx="6" cy="19" r="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><circle cx="18" cy="7" r="2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M6 7v10m12-8c0 5-6 4-11 8" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
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

/// A pill (`ControlSize::Small` ghost button): 10 pt each side.
const PILL_PAD: f32 = 20.0;
/// An icon-only pill: 14 pt glyph and 3 pt each side.
const ICON_PILL: f32 = 20.0;
const GLYPH: f32 = 14.0;
/// Between pills in the row.
const ROW_GAP: f32 = 2.0;
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
    auto_icon: bool,
    mode_icon: bool,
    branch_icon: bool,
    more_branch: bool,
    more_auto: bool,
    more_mode: bool,
    more_agent: bool,
}

const FULL: Fold = Fold {
    agent_icon: false,
    model: ModelLook::Split,
    auto_icon: false,
    mode_icon: false,
    branch_icon: false,
    more_branch: false,
    more_auto: false,
    more_mode: false,
    more_agent: false,
};

/// The steps, in the order the row gives them up: words before meaning;
/// the permission mode (a bypass is worth seeing) is the last to leave, after
/// the agent, which the pane's header names anyway.
const FOLDS: [fn(&mut Fold); 12] = [
    |f| f.agent_icon = true,
    |f| f.model = ModelLook::Joined,
    |f| f.auto_icon = true,
    |f| f.mode_icon = true,
    |f| f.branch_icon = true,
    |f| f.model = ModelLook::Name,
    |f| f.model = ModelLook::Short,
    |f| f.more_branch = true,
    |f| f.model = ModelLook::Icon,
    |f| f.more_auto = true,
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
    current: Option<(String, String)>,
}

/// The open session's controls, read once per frame.
pub struct Controls {
    language: Language,
    dark: bool,
    agent_id: String,
    agent_name: String,
    model: Option<ModelChip>,
    branch: Option<(String, BranchInfo)>,
    modes: Vec<(String, String)>,
    mode: Option<String>,
    asid: muqun_gateway::agents::AgentSessionId,
    auto_on: bool,
    busy: bool,
}

impl Controls {
    pub fn read(s: &UiState) -> Option<Controls> {
        let info = s.current()?;
        let catalog = s.catalog.as_ref();
        let model_infos: Vec<&muqun_gateway::agents::ModelInfo> = catalog
            .map(|c| c.catalog.models.iter().filter(|m| m.enabled).collect())
            .unwrap_or_default();
        let models = catalog.map(|c| catalog_models(c)).unwrap_or_default();
        let default_model = catalog.and_then(|c| c.catalog.defaults.model.as_ref());
        let model = match &info.model {
            Some(m) => {
                let selected = super::labels::catalog_model_info(&model_infos, m, default_model);
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
                    current: Some((m.provider_id.clone(), m.model_id.clone())),
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
        let modes: Vec<(String, String)> = catalog
            .map(|c| {
                c.catalog
                    .modes
                    .iter()
                    .filter(|m| !m.hidden)
                    .map(|m| (m.id.clone(), mode_label(&m.id, Some(m.name.as_str()))))
                    .collect()
            })
            .unwrap_or_default();
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
            auto_on: s.auto_approve.get(&info.asid.0).copied().unwrap_or(false),
            busy: s.busy(),
        })
    }

    fn tr(&self, en: &'static str, zh: &'static str) -> &'static str {
        tr_ref(self.language, en, zh)
    }

    /// The current mode's id (as the catalogue spells it) and label.
    fn current_mode(&self) -> Option<(String, String)> {
        let mode = self.mode.as_ref()?;
        Some(
            self.modes
                .iter()
                .find(|(id, _)| id == mode || bare_catalog_id(id) == mode.as_str())
                .cloned()
                .unwrap_or_else(|| (mode.clone(), mode_label(mode, None))),
        )
    }

    /// 2–4 modes read best as a segmented control while there is room.
    fn segmented(&self) -> bool {
        (2..=4).contains(&self.modes.len())
    }

    fn branch_label(&self) -> Option<String> {
        let (_, info) = self.branch.as_ref()?;
        info.current.as_ref().map(|b| format!("⎇ {b}"))
    }

    /// Whether anything goes into the "…" menu at `f`.
    fn has_more(&self, f: Fold) -> bool {
        (f.more_branch && self.branch.is_some())
            || f.more_auto
            || (f.more_mode && (self.mode.is_some() || self.modes.len() > 1))
            || f.more_agent
    }

    /// The row's width at fold `f`, as it will lay out.
    fn width(&self, k: &Kit, f: Fold) -> f32 {
        let small = k.ty.small;
        let mut items: Vec<f32> = Vec::new();
        if !f.more_agent {
            items.push(if f.agent_icon {
                ICON_PILL
            } else {
                8.0 + 12.0 + 6.0 + text_w(&self.agent_name, k.ty.small_medium) + 8.0
            });
        }
        if let Some(m) = &self.model {
            match f.model {
                ModelLook::Split => {
                    items.push(text_w(&m.name, small) + PILL_PAD);
                    if let Some(e) = &m.effort {
                        items.push(text_w(e, small) + PILL_PAD);
                    }
                }
                ModelLook::Joined => items.push(text_w(&joined(m), small) + PILL_PAD),
                ModelLook::Name => items.push(text_w(&m.name, small) + PILL_PAD),
                ModelLook::Short => items.push(text_w(short_name(&m.name), small) + PILL_PAD),
                ModelLook::Icon => items.push(ICON_PILL),
            }
        }
        if let (Some(label), false) = (self.branch_label(), f.more_branch) {
            items.push(if f.branch_icon {
                ICON_PILL
            } else {
                text_w(&label, small) + PILL_PAD
            });
        }
        if !f.more_mode {
            if f.mode_icon {
                if self.mode.is_some() || self.modes.len() > 1 {
                    items.push(ICON_PILL);
                }
            } else if self.segmented() {
                let widest = self
                    .modes
                    .iter()
                    .map(|(_, n)| text_w(n, k.ty.ui))
                    .fold(0.0, f32::max);
                items.push((widest + 24.0) * self.modes.len() as f32);
            } else if self.mode.is_some() || self.modes.len() > 1 {
                let label = self
                    .current_mode()
                    .map_or_else(|| self.tr("Mode", "模式").to_string(), |(_, l)| l);
                items.push(GLYPH + 4.0 + text_w(&label, small) + PILL_PAD);
            }
        }
        if !f.more_auto {
            items.push(if f.auto_icon {
                ICON_PILL
            } else {
                // The switch's label, its gap and the small track.
                text_w(self.tr("Auto-approve", "自动批准"), small) + 8.0 + 44.0
            });
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
        .on_press(move |s: &mut UiState| s.menu = (!open).then(|| menu.clone()));
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
        e = e.px(3.0);
    }
    let chip_fill = if open {
        k.c.bg_accent.alpha(0.14)
    } else {
        k.c.layer_01.alpha(0.35)
    };
    let chip_border = if open {
        k.c.bg_accent.alpha(0.55)
    } else {
        k.c.border_muted.alpha(0.30)
    };
    e.radius(k.radius.xs)
        .fill(chip_fill)
        .border(0.5, chip_border)
}

fn agent_chip(c: &Controls, k: &Kit, f: Fold) -> El {
    let mark = super::agents::mark(&c.agent_id, 12.0, c.dark).a11y_hidden();
    let mut chip = row()
        .key("agent")
        .role("image")
        .label(c.agent_name.clone())
        .center_y()
        .h(22.0)
        .radius(k.radius.xs)
        .fill(k.c.layer_01.alpha(0.35))
        .border(0.5, k.c.border_muted.alpha(0.30))
        .child(mark);
    if f.agent_icon {
        return chip.w(ICON_PILL).center().help(c.agent_name.clone());
    }
    chip = chip.gap(5.0).px(7.0).child(
        text(c.agent_name.clone())
            .font(k.ty.small_medium)
            .color(k.c.text_muted)
            .a11y_hidden(),
    );
    if c.busy {
        chip = chip.child(
            rune_kit::Spinner::new()
                .size(8.0)
                .color(k.c.bg_accent)
                .build(k),
        );
    }
    chip
}

/// Model choices, then (when the model has them) its effort levels.
fn model_items(menu: Menu, c: &Controls, m: &ModelChip, with_effort: bool) -> Menu {
    let mut menu = menu;
    let mut last: Option<&str> = None;
    for (provider, id, name) in &m.models {
        if last != Some(provider.as_str()) {
            menu = menu.group_label(provider.clone());
            last = Some(provider.as_str());
        }
        let on = m.current.as_ref() == Some(&(provider.clone(), id.clone()));
        let (p, id) = (provider.clone(), id.clone());
        menu = menu.item(
            MenuItem::new(name.clone())
                .checked(on)
                .on_select(move |s: &mut UiState| switch_model(s, p.clone(), id.clone())),
        );
    }
    if with_effort && !m.variants.is_empty() {
        if !m.models.is_empty() {
            menu = menu.separator();
        }
        menu = menu.group_label(c.tr("Effort", "推理强度")).item(
            MenuItem::new(c.tr("Default", "默认"))
                .checked(m.variant.is_none())
                .on_select(|s: &mut UiState| switch_effort(s, None)),
        );
        for v in &m.variants {
            let name = v.clone();
            menu = menu.item(
                MenuItem::new(readable_catalog_id(v))
                    .checked(m.variant.as_deref() == Some(v.as_str()))
                    .on_select(move |s: &mut UiState| switch_effort(s, Some(name.clone()))),
            );
        }
    }
    menu
}

fn model_chips(s: &UiState, c: &Controls, k: &Kit, layer: &mut Vec<El>, f: Fold, line: El) -> El {
    let Some(m) = &c.model else { return line };
    let open = s.menu == Some(OpenMenu::Model);
    if f.model == ModelLook::Split {
        let mut line = line;
        line = line.child(if m.models.is_empty() {
            Badge::new(m.name.clone())
                .variant(BadgeVariant::Subtle)
                .build(k)
                .key("model")
        } else {
            let current = m.current.as_ref().map(|(p, i)| (p.as_str(), i.as_str()));
            model_menu(
                k,
                layer,
                OpenMenu::Model,
                open,
                m.name.clone(),
                &m.models,
                current,
                switch_model,
            )
        });
        if !m.variants.is_empty() {
            line = line.child(effort_menu(
                k,
                layer,
                s.menu == Some(OpenMenu::Effort),
                m.variant.as_deref(),
                &m.variants,
            ));
        }
        return line;
    }
    let whole = joined(m);
    let help = format!("{} {whole}", c.tr("Model:", "模型："));
    let face = match f.model {
        ModelLook::Joined => Face::Text(whole.clone()),
        ModelLook::Name => Face::Text(m.name.clone()),
        ModelLook::Short => Face::Text(short_name(&m.name).to_string()),
        _ => Face::Glyph(&MODEL, None),
    };
    if m.models.is_empty() && m.variants.is_empty() {
        let badge = Badge::new(if f.model == ModelLook::Icon {
            m.name.clone()
        } else {
            whole
        })
        .variant(BadgeVariant::Subtle)
        .build(k)
        .key("model");
        return line.child(badge);
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
    let menu = Menu::new("model")
        .placement(rune_kit::Placement::TopStart)
        .trigger(trigger)
        .open(open)
        .min_width(220.0)
        .on_close(|s: &mut UiState| s.menu = None);
    line.child(model_items(menu, c, m, true).build(k, layer))
}

fn mode_items(menu: Menu, c: &Controls, k: &Kit) -> Menu {
    let current = c.current_mode().map(|(id, _)| id);
    let mut menu = menu;
    for (id, label) in &c.modes {
        let (glyph, warn) = mode_glyph(id);
        let pick = id.clone();
        let mut item = MenuItem::new(label.clone())
            .lead(icon(glyph, GLYPH).color(if warn {
                k.c.warning.icon
            } else {
                k.c.icon_muted
            }))
            .checked(current.as_deref() == Some(id.as_str()))
            .on_select(move |s: &mut UiState| switch_mode(s, pick.clone()));
        if warn {
            item = item.description(c.tr("Nothing asks first", "不经询问直接执行"));
        }
        menu = menu.item(item);
    }
    menu
}

fn mode_control(s: &UiState, c: &Controls, k: &Kit, layer: &mut Vec<El>, f: Fold) -> Option<El> {
    if !f.mode_icon && c.segmented() {
        let labels: Vec<&str> = c.modes.iter().map(|(_, n)| n.as_str()).collect();
        let at = c.mode.as_ref().and_then(|m| {
            c.modes
                .iter()
                .position(|(id, _)| id == m || bare_catalog_id(id) == m.as_str())
        });
        let ids: Vec<String> = c.modes.iter().map(|(id, _)| id.clone()).collect();
        return Some(
            SegmentedControl::new(&labels, at)
                .fit()
                .label(c.tr("Mode", "模式"))
                .on_pick(move |s: &mut UiState, i, _: &mut Tx| switch_mode(s, ids[i].clone()))
                .build(k)
                .key("modes"),
        );
    }
    let (id, label) = c
        .current_mode()
        .unwrap_or_else(|| (String::new(), c.tr("Mode", "模式").to_string()));
    let (glyph, warn) = mode_glyph(&id);
    let tone = warn.then_some(k.c.warning.icon);
    let help = format!("{} {label}", c.tr("Permission mode:", "权限模式："));
    if c.modes.len() < 2 {
        // Nothing to switch to: say what it is.
        c.mode.as_ref()?;
        let chip = row()
            .key("mode")
            .role("image")
            .label(help.clone())
            .help(help)
            .h(20.0)
            .center_y()
            .child(
                icon(glyph, GLYPH)
                    .color(tone.unwrap_or(k.c.icon_muted))
                    .a11y_hidden(),
            );
        return Some(if f.mode_icon {
            chip.w(ICON_PILL).center()
        } else {
            chip.gap(4.0).px(6.0).child(
                text(label)
                    .font(k.ty.small)
                    .color(tone.unwrap_or(k.c.text_muted))
                    .a11y_hidden(),
            )
        });
    }
    let open = s.menu == Some(OpenMenu::Mode);
    let face = if f.mode_icon {
        Face::Glyph(glyph, tone)
    } else {
        Face::Labelled(glyph, tone, label)
    };
    let trigger = pill_button(
        k,
        "mode-pill",
        face,
        help.clone(),
        Some(help),
        open,
        OpenMenu::Mode,
    );
    let menu = Menu::new("mode")
        .placement(rune_kit::Placement::TopStart)
        .trigger(trigger)
        .open(open)
        .min_width(200.0)
        .on_close(|s: &mut UiState| s.menu = None);
    Some(mode_items(menu, c, k).build(k, layer))
}

fn set_auto(s: &mut UiState, asid: &muqun_gateway::agents::AgentSessionId, on: bool) {
    s.send(Command::Compose(ComposeCommand::SetAutoApprove {
        asid: asid.clone(),
        enabled: on,
    }));
}

fn auto_help(c: &Controls) -> String {
    format!(
        "{} · {}",
        c.tr("Auto-approve", "自动批准"),
        if c.auto_on {
            c.tr(
                "on: requests are approved without asking",
                "已开：请求不再询问，直接批准",
            )
        } else {
            c.tr("off: requests wait for you", "已关：请求等你批准")
        }
    )
}

fn auto_control(c: &Controls, k: &Kit, f: Fold) -> El {
    let (on, asid) = (c.auto_on, c.asid.clone());
    let label = c.tr("Auto-approve", "自动批准");
    if !f.auto_icon {
        return rune_kit::Switch::new(on)
            .label(label)
            .on_toggle(move |s: &mut UiState| set_auto(s, &asid, !on))
            .build(k)
            .key("auto-approve");
    }
    // A toggle: lit in the accent while on.
    let tone = if on { k.c.bg_accent } else { k.c.icon_muted };
    let b = Button::new(label)
        .variant(ButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .help(auto_help(c))
        .label_with(move |_, _| icon(&AUTO_APPROVE, GLYPH).color(tone))
        .on_press(move |s: &mut UiState| set_auto(s, &asid, !on))
        .build(k)
        .key("auto-approve")
        .role("switch")
        .checked(on)
        .px(3.0);
    if on {
        b.fill(k.c.bg_accent.alpha(0.14))
    } else {
        b
    }
}

fn branch_control(s: &UiState, c: &Controls, k: &Kit, layer: &mut Vec<El>, f: Fold) -> Option<El> {
    let (cwd, info) = c.branch.as_ref()?;
    if !f.branch_icon {
        return super::composer::branch_menu(
            k,
            layer,
            OpenMenu::Branch,
            s.menu == Some(OpenMenu::Branch),
            cwd,
            info,
        );
    }
    let current = info.current.clone()?;
    let help = format!("{} {current}", c.tr("Branch:", "分支："));
    if info.local.len() < 2 {
        return Some(
            row()
                .key("branch")
                .role("image")
                .label(help.clone())
                .help(help)
                .w(ICON_PILL)
                .h(20.0)
                .center()
                .child(icon(&BRANCH, GLYPH).color(k.c.icon_muted).a11y_hidden()),
        );
    }
    let open = s.menu == Some(OpenMenu::Branch);
    let trigger = pill_button(
        k,
        "branch-pill",
        Face::Glyph(&BRANCH, None),
        help.clone(),
        Some(help),
        open,
        OpenMenu::Branch,
    );
    let menu = Menu::new("branch")
        .placement(rune_kit::Placement::TopStart)
        .trigger(trigger)
        .open(open)
        .on_close(|s: &mut UiState| s.menu = None);
    Some(branch_items(menu, cwd, info).build(k, layer))
}

/// The "…" menu: whatever the fold moved out of the row.
fn more_menu(s: &UiState, c: &Controls, k: &Kit, layer: &mut Vec<El>, f: Fold) -> El {
    let open = s.menu == Some(OpenMenu::ComposerMore);
    let label = c.tr("More controls", "更多控制");
    let trigger = pill_button(
        k,
        "composer-more-pill",
        Face::Glyph(&MORE, None),
        label.to_string(),
        Some(label.to_string()),
        open,
        OpenMenu::ComposerMore,
    );
    let mut menu = Menu::new("composer-more")
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
                .lead(super::agents::mark(&c.agent_id, 12.0, c.dark))
                .disabled(true),
        );
    }
    if f.more_mode && c.modes.len() > 1 {
        menu = mode_items(
            section(menu).group_label(c.tr("Permission mode", "权限模式")),
            c,
            k,
        );
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
    if f.more_auto {
        let (on, asid) = (c.auto_on, c.asid.clone());
        menu = section(menu).item(
            MenuItem::new(c.tr("Auto-approve", "自动批准"))
                .checked(on)
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    set_auto(s, &asid, !on);
                }),
        );
    }
    if let (true, Some((cwd, info))) = (f.more_branch, c.branch.as_ref()) {
        menu = section(menu).group_label(c.tr("Branch", "分支"));
        menu = if info.local.len() < 2 {
            menu.item(
                MenuItem::new(info.current.clone().unwrap_or_default())
                    .checked(true)
                    .disabled(true),
            )
        } else {
            branch_items(menu, cwd, info)
        };
    }
    menu.build(k, layer)
}

fn build(s: &UiState, c: &Controls, k: &Kit, layer: &mut Vec<El>, f: Fold) -> El {
    let mut line = row().key("pills").gap(ROW_GAP).center_y();
    if !f.more_agent {
        line = line.child(agent_chip(c, k, f));
    }
    line = model_chips(s, c, k, layer, f, line);
    if !f.more_branch {
        if let Some(b) = branch_control(s, c, k, layer, f) {
            line = line.child(b);
        }
    }
    if !f.more_mode {
        if let Some(m) = mode_control(s, c, k, layer, f) {
            line = line.child(m);
        }
    }
    if !f.more_auto {
        line = line.child(auto_control(c, k, f));
    }
    if c.has_more(f) {
        line = line.child(more_menu(s, c, k, layer, f));
    }
    line
}

/// The control row for `room` points beside the send buttons. `None` when
/// even the narrowest fold does not fit there: the caller gives the row a
/// line of its own ([`own_line`]).
pub fn beside(s: &UiState, c: &Controls, k: &Kit, layer: &mut Vec<El>, room: f32) -> Option<El> {
    let f = c.fit(k, room)?;
    Some(build(s, c, k, layer, f))
}

/// The row on a line of its own, `room` points wide.
pub fn own_line(s: &UiState, c: &Controls, k: &Kit, layer: &mut Vec<El>, room: f32) -> El {
    let f = c.fit(k, room).unwrap_or_else(|| fold_at(FOLDS.len()));
    build(s, c, k, layer, f)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                current: Some(("anthropic".into(), "opus-5.5".into())),
            }),
            branch: Some((
                "/w".into(),
                BranchInfo {
                    current: Some("main".into()),
                    local: vec!["main".into(), "next".into()],
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
            .map(|m| (m.to_string(), readable_catalog_id(m)))
            .collect(),
            mode: Some("bypassPermissions".into()),
            asid: muqun_gateway::agents::AgentSessionId("s".into()),
            auto_on: true,
            busy: false,
        }
    }

    /// Less room never shows more: each fold is no wider than the one
    /// before, so the row only ever folds further as a pane narrows.
    #[test]
    fn narrower_room_never_unfolds() {
        let (k, c) = (Kit::neutral(false), claude());
        let widths: Vec<f32> = (0..=FOLDS.len()).map(|l| c.width(&k, fold_at(l))).collect();
        assert!(widths.windows(2).all(|w| w[1] <= w[0]), "{widths:?}");
        assert!(
            widths[0] > 400.0 && widths[FOLDS.len()] < 70.0,
            "{widths:?}"
        );
    }
}
