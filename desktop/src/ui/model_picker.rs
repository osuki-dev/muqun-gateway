//! App-style two-column, searchable, provider-grouped model picker shared by
//! Home and session composers.
//!
//! Mirrors Muqun App's `AgentModelSheet`:
//! - Header showing title and current `Model · Effort` summary
//! - Search input matching model names, IDs, and provider names
//! - `All models` / `Free only` segmented filter (shown only when free models exist)
//! - Left provider rail (`Recently used` + providers in catalog order) when the
//!   catalog has multiple providers
//! - Right scrollable model list with context window + effort range captions,
//!   `Free` badge, unavailable hints, and inline effort chips directly under
//!   the currently selected model.

use std::collections::{HashMap, HashSet};

use muqun_gateway::agents::{order_efforts, ModelInfo, ModelRef, ProviderInfo};
use rune::{col, el, icon, row, text, Align, El, ScrollSpec, Side, TextEdit, Transition};
use rune_kit::{Kit, TextInput};

use super::copy::tr_ref;
use super::{OpenMenu, Slot, UiState};
use crate::config::Language;
use crate::hub::CatalogView;

const RECENT_PROVIDER_ID: &str = "__recent__";
const PANEL_W_TWO_COL: f32 = 504.0;
const PANEL_W_ONE_COL: f32 = 320.0;
const PANEL_PAD: f32 = 12.0;
const PROVIDER_RAIL_W: f32 = 148.0;
const LIST_MAX_H: f32 = 380.0;
const MAX_RECENT_ITEMS: usize = 5;

/// Ephemeral UI state for the open model picker popover.
#[derive(Debug, Default, Clone)]
pub struct PickerState {
    pub filter_free: bool,
    pub picked_provider: Option<String>,
}

impl PickerState {
    pub fn reset(&mut self) {
        self.filter_free = false;
        self.picked_provider = None;
    }
}

impl UiState {
    pub(super) fn toggle_composer_menu(&mut self, kind: OpenMenu) {
        let opening = self.menu.as_ref() != Some(&kind);
        if opening && matches!(kind, OpenMenu::Model | OpenMenu::StartModel) {
            self.model_query = TextEdit::new("");
            self.model_picker.reset();
        }
        self.menu = opening.then_some(kind);
    }
}

fn close(s: &mut UiState) {
    s.menu = None;
    s.model_query = TextEdit::new("");
    s.model_picker.reset();
}

// ---- pure catalog helpers (parity with App's agent-model-groups/protocol) ----

/// Read `Model.Cost` tiers (`{input, output}` object or array of objects).
fn read_cost_tiers(cost: Option<&serde_json::Value>) -> Option<Vec<(f64, f64)>> {
    let value = cost?;
    let items: Vec<&serde_json::Value> = match value {
        serde_json::Value::Array(arr) => arr.iter().collect(),
        serde_json::Value::Object(_) => vec![value],
        _ => return None,
    };
    let mut tiers = Vec::new();
    for entry in items {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let (Some(input), Some(output)) = (
            obj.get("input").and_then(serde_json::Value::as_f64),
            obj.get("output").and_then(serde_json::Value::as_f64),
        ) else {
            continue;
        };
        tiers.push((input, output));
    }
    (!tiers.is_empty()).then_some(tiers)
}

/// Whether a catalog model sits on the free tier.
///
/// Price list wins whenever present (all tiers have `input == 0 && output == 0`);
/// only when no price list is published does the model id/name get a say.
pub(super) fn is_free_model(model: &ModelInfo) -> bool {
    if let Some(tiers) = read_cost_tiers(model.cost.as_ref()) {
        return tiers
            .iter()
            .all(|(input, output)| *input == 0.0 && *output == 0.0);
    }
    let id_lower = model.id.to_ascii_lowercase();
    let name_lower = model.name.to_ascii_lowercase();
    id_lower.contains("free") || name_lower.contains("free")
}

fn has_free_model(models: &[ModelInfo]) -> bool {
    models.iter().any(is_free_model)
}

fn has_multiple_providers(models: &[ModelInfo], providers: &[ProviderInfo]) -> bool {
    let mut ids: HashSet<&str> = providers.iter().map(|p| p.id.as_str()).collect();
    for model in models {
        ids.insert(if model.provider_id.is_empty() {
            "other"
        } else {
            model.provider_id.as_str()
        });
    }
    ids.len() > 1
}

fn provider_title(providers: &[ProviderInfo], provider_id: &str) -> String {
    providers
        .iter()
        .find(|p| p.id == provider_id)
        .map(|p| p.name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| provider_id.to_owned())
}

fn display_model_name(model: &ModelInfo) -> String {
    if !model.name.trim().is_empty() && model.name != model.id {
        return model.name.clone();
    }
    super::labels::format_model_name(
        &ModelRef {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            variant: None,
        },
        &model.id,
        None,
    )
}

fn sorted_variant_ids(model: &ModelInfo) -> Vec<String> {
    let mut ids: Vec<String> = model
        .variants
        .as_ref()
        .map(|list| list.iter().map(|v| v.id.clone()).collect())
        .unwrap_or_default();
    order_efforts(&mut ids, |id| Some(id.as_str()));
    ids
}

/// Subtitle under a model name: context size and effort range, e.g.
/// `1.0M context · low to max`.
pub(super) fn model_caption(model: &ModelInfo) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(context) = model
        .limit
        .as_ref()
        .and_then(|l| l.get("context"))
        .and_then(serde_json::Value::as_f64)
        .filter(|&c| c > 0.0)
    {
        if context >= 1_000_000.0 {
            parts.push(format!("{:.1}M context", context / 1_000_000.0));
        } else if context >= 1_000.0 {
            parts.push(format!("{:.0}k context", context / 1_000.0));
        } else {
            parts.push(format!("{:.0} context", context));
        }
    }
    let variants = sorted_variant_ids(model);
    if variants.len() == 1 {
        parts.push(variants[0].clone());
    } else if variants.len() > 1 {
        parts.push(format!(
            "{} to {}",
            variants[0],
            variants[variants.len() - 1]
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

struct ProviderSection<'a> {
    provider_id: String,
    title: String,
    models: Vec<&'a ModelInfo>,
    recent: bool,
    unavailable: bool,
}

fn recent_catalog_models<'a>(
    recent_keys: &[String],
    models: &[&'a ModelInfo],
    providers: &[ProviderInfo],
) -> Vec<&'a ModelInfo> {
    let disabled: HashSet<&str> = providers
        .iter()
        .filter(|p| p.activation.as_deref() == Some("disabled") || !p.available)
        .map(|p| p.id.as_str())
        .collect();
    let mut out: Vec<&'a ModelInfo> = Vec::new();
    for entry in recent_keys {
        let Some((provider_id, model_id)) = entry.split_once('/') else {
            continue;
        };
        let Some(&model) = models
            .iter()
            .find(|m| m.provider_id == provider_id && m.id == model_id)
        else {
            continue;
        };
        if !model.enabled || disabled.contains(model.provider_id.as_str()) {
            continue;
        }
        if out
            .iter()
            .any(|existing| existing.provider_id == model.provider_id && existing.id == model.id)
        {
            continue;
        }
        out.push(model);
        if out.len() >= MAX_RECENT_ITEMS {
            break;
        }
    }
    out
}

fn group_models_by_provider<'a>(
    models: &[&'a ModelInfo],
    providers: &[ProviderInfo],
    include_empty_unavailable: bool,
) -> Vec<ProviderSection<'a>> {
    let mut by_provider: HashMap<&str, Vec<&'a ModelInfo>> = HashMap::new();
    let mut model_provider_order: Vec<&str> = Vec::new();
    for &model in models {
        let pid = if model.provider_id.is_empty() {
            "other"
        } else {
            model.provider_id.as_str()
        };
        if !by_provider.contains_key(pid) {
            model_provider_order.push(pid);
        }
        by_provider.entry(pid).or_default().push(model);
    }

    let mut ordered_ids: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for p in providers {
        if seen.insert(p.id.as_str()) {
            ordered_ids.push(p.id.as_str());
        }
    }
    for pid in model_provider_order {
        if seen.insert(pid) {
            ordered_ids.push(pid);
        }
    }

    let mut groups = Vec::new();
    for pid in ordered_ids {
        let provider = providers.iter().find(|p| p.id == pid);
        let available = provider.is_none_or(|p| p.available);
        let group_models = by_provider.remove(pid).unwrap_or_default();
        if group_models.is_empty() && (available || !include_empty_unavailable) {
            continue;
        }
        groups.push(ProviderSection {
            provider_id: pid.to_owned(),
            title: provider_title(providers, pid),
            models: group_models,
            recent: false,
            unavailable: !available,
        });
    }
    groups
}

fn fallback_models(models: &[(String, String, String)]) -> Vec<ModelInfo> {
    models
        .iter()
        .map(|(provider_id, id, name)| ModelInfo {
            id: id.clone(),
            name: name.clone(),
            provider_id: provider_id.clone(),
            family: None,
            limit: None,
            variants: None,
            cost: None,
            enabled: true,
            status: None,
        })
        .collect()
}

// ---- visual building blocks --------------------------------------------------

fn free_chip(k: &Kit, label: &str) -> El {
    row()
        .px(7.0)
        .py(2.0)
        .radius(k.radius.lg)
        .fill(k.c.success.icon.alpha(0.14))
        .center_y()
        .child(
            text(label.to_owned())
                .font(k.ty.small_medium)
                .color(k.c.success.fg),
        )
}

fn free_filter_bar(k: &Kit, lang: Language, filter_free: bool) -> El {
    let segment = |key: &str, label: &str, active: bool, next_free: bool| {
        let mut btn = row()
            .key(key.to_owned())
            .grow(1.0)
            .min_w(0.0)
            .h(28.0)
            .px(10.0)
            .radius(k.radius.md)
            .center()
            .focusable(true)
            .role("radio")
            .checked(active)
            .label(label.to_owned())
            .child(
                text(label.to_owned())
                    .font(if active {
                        k.ty.small_medium
                    } else {
                        k.ty.small
                    })
                    .color(if active {
                        k.c.text_strong
                    } else {
                        k.c.text_muted
                    })
                    .truncate(),
            );
        if active {
            btn = btn
                .fill(k.c.surface_raised_base)
                .border(1.0, k.c.border_base.alpha(0.6));
        }
        k.interactive(btn).on_press(move |s: &mut UiState| {
            s.model_picker.filter_free = next_free;
        })
    };

    row()
        .key("model-free-filter")
        .role("radiogroup")
        .label(tr_ref(lang, "Filter models"))
        .w_full()
        .padding(3.0)
        .gap(3.0)
        .radius(k.radius.lg)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak.alpha(0.6))
        .center_y()
        .child(segment(
            "model-filter-all",
            tr_ref(lang, "All models"),
            !filter_free,
            false,
        ))
        .child(segment(
            "model-filter-free",
            tr_ref(lang, "Free only"),
            filter_free,
            true,
        ))
}

fn variant_chip(
    k: &Kit,
    key: String,
    label: String,
    active: bool,
    on_press: impl Fn(&mut UiState) + 'static,
) -> El {
    let fill = if active {
        k.c.bg_accent
    } else {
        k.c.bg_accent.alpha(0.10)
    };
    let fg = if active {
        k.c.text_contrast
    } else {
        k.c.text_accent
    };
    let font = if active {
        k.ty.small_medium
    } else {
        k.ty.small
    };
    let chip = row()
        .key(key)
        .px(10.0)
        .py(4.0)
        .radius(k.radius.lg)
        .fill(fill)
        .center_y()
        .focusable(true)
        .role("button")
        .checked(active)
        .label(label.clone())
        .child(text(label).font(font).color(fg));
    k.interactive(chip).on_press(on_press)
}

fn provider_rail_item(k: &Kit, section: &ProviderSection<'_>, selected: bool) -> El {
    let pid = section.provider_id.clone();
    let bar_color = if selected {
        k.c.bg_accent
    } else {
        k.c.bg_accent.alpha(0.0)
    };
    let bg = if selected {
        k.c.bg_accent.alpha(0.10)
    } else {
        k.c.bg_accent.alpha(0.0)
    };
    let fg = if selected {
        k.c.text_accent
    } else if section.unavailable {
        k.c.text_faint
    } else {
        k.c.text_muted
    };
    let font = if selected {
        k.ty.small_medium
    } else {
        k.ty.small
    };
    let item = row()
        .key(format!("model-provider-{}", section.provider_id))
        .w_full()
        .min_h(36.0)
        .radius(k.radius.md)
        .fill(bg)
        .center_y()
        .focusable(true)
        .role("tab")
        .checked(selected)
        .label(section.title.clone())
        .child(
            el().w(2.5)
                .h(20.0)
                .radius(k.radius.xs)
                .fill(bar_color)
                .a11y_hidden(),
        )
        .child(
            text(section.title.clone())
                .grow(1.0)
                .min_w(0.0)
                .px(8.0)
                .py(6.0)
                .font(font)
                .color(fg)
                .wrap(),
        );
    k.interactive(item).on_press(move |s: &mut UiState| {
        s.model_picker.picked_provider = Some(pid.clone());
    })
}

#[allow(clippy::too_many_arguments)]
fn model_row(
    k: &Kit,
    lang: Language,
    key: String,
    model: &ModelInfo,
    caption: Option<String>,
    selected: bool,
    unavailable: bool,
    unavailable_hint: Option<String>,
    active_variant: Option<&str>,
    slot: Slot,
    on_pick: impl Fn(&mut UiState) + 'static,
) -> El {
    let title = display_model_name(model);
    let variants = sorted_variant_ids(model);
    let show_free = !unavailable && is_free_model(model);
    let show_variants = selected && !unavailable && !variants.is_empty();

    let title_color = if unavailable {
        k.c.text_faint
    } else if selected {
        k.c.text_accent
    } else {
        k.c.text_base
    };
    let title_font = if selected && !unavailable {
        k.ty.ui_medium
    } else {
        k.ty.ui
    };

    let mut title_line = row().w_full().gap(8.0).center_y().child(
        text(title.clone())
            .grow(1.0)
            .min_w(0.0)
            .font(title_font)
            .color(title_color)
            .truncate(),
    );
    if show_free {
        title_line = title_line.child(free_chip(k, tr_ref(lang, "Free")));
    } else if selected && !unavailable && !show_variants {
        title_line = title_line.child(
            icon(&super::parts::glyphs::DONE, 14.0)
                .color(k.c.icon_accent)
                .a11y_hidden(),
        );
    }

    let mut content = col().w_full().gap(2.0).child(title_line);

    let sub = if unavailable {
        unavailable_hint
    } else {
        caption
    };
    if let Some(sub) = sub {
        content = content.child(
            text(sub)
                .font(k.ty.small)
                .color(if unavailable {
                    k.c.text_faint
                } else {
                    k.c.text_muted
                })
                .wrap()
                .w_full(),
        );
    }

    let mut main_row = row()
        .key(key.clone())
        .w_full()
        .radius(k.radius.md)
        .px(10.0)
        .py(7.0)
        .focusable(!unavailable)
        .role("menuitemradio")
        .checked(selected && !unavailable)
        .label(title.clone())
        .help_when_truncated(title)
        .child(content);

    if !unavailable {
        main_row = k.interactive(main_row).on_press(on_pick);
    }

    if !show_variants {
        return if selected && !unavailable {
            col()
                .key(format!("{key}-wrap"))
                .w_full()
                .radius(k.radius.md)
                .fill(k.c.bg_accent.alpha(0.08))
                .child(main_row)
        } else {
            main_row
        };
    }

    let mut chips = row()
        .key(format!("{key}-variants"))
        .w_full()
        .px(10.0)
        .pt(2.0)
        .pb(8.0)
        .gap(6.0)
        .flex_wrap()
        .center_y();
    for variant_id in variants {
        let is_active = active_variant == Some(variant_id.as_str());
        let label = super::labels::readable_catalog_id(&variant_id);
        let next_variant = if is_active {
            None
        } else {
            Some(variant_id.clone())
        };
        let chip_key = format!("{key}-variant-{variant_id}");
        chips = chips.child(variant_chip(k, chip_key, label, is_active, move |s| {
            close(s);
            super::composer::switch_effort(s, slot, next_variant.clone());
        }));
    }

    col()
        .key(format!("{key}-wrap"))
        .w_full()
        .radius(k.radius.md)
        .fill(k.c.bg_accent.alpha(0.08))
        .child(main_row)
        .child(chips)
}

// ---- popup entry point -------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(super) fn popup(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    key: &str,
    trigger: El,
    open: bool,
    slot: Slot,
    agent_id: &str,
    catalog: Option<&CatalogView>,
    fallback: &[(String, String, String)],
    current: Option<&ModelRef>,
    on_pick: impl Fn(&mut UiState, String, String) + Clone + 'static,
) -> El {
    let lang = s.cfg.general.language;
    let search_placeholder = tr_ref(lang, "Search models or providers");
    let anchor_key = format!("{key}-trigger");
    let anchored_trigger = el().key(anchor_key.clone()).child(trigger);
    if !open {
        return anchored_trigger;
    }

    let synthetic_models;
    let (all_models, providers, default_model): (&[ModelInfo], &[ProviderInfo], Option<&ModelRef>) =
        match catalog {
            Some(view) if !view.catalog.models.is_empty() || !view.catalog.providers.is_empty() => {
                (
                    view.catalog.models.as_slice(),
                    view.catalog.providers.as_slice(),
                    view.catalog.defaults.model.as_ref(),
                )
            }
            _ => {
                synthetic_models = fallback_models(fallback);
                (synthetic_models.as_slice(), &[], None)
            }
        };

    let effective = current.or(default_model);
    let show_filter = has_free_model(all_models);
    let filter_free = show_filter && s.model_picker.filter_free;
    let show_provider_rail = has_multiple_providers(all_models, providers);
    let panel_w = if show_provider_rail {
        PANEL_W_TWO_COL
    } else {
        PANEL_W_ONE_COL
    };
    let inner_w = panel_w - PANEL_PAD * 2.0;

    let terms = s.model_query.text.trim().to_ascii_lowercase();
    let searching = !terms.is_empty() || filter_free;

    let filtered_models: Vec<&ModelInfo> = all_models
        .iter()
        .filter(|m| !filter_free || is_free_model(m))
        .filter(|m| {
            if terms.is_empty() {
                return true;
            }
            let p_title = provider_title(providers, &m.provider_id);
            let searchable =
                format!("{} {} {} {}", m.provider_id, p_title, m.id, m.name).to_ascii_lowercase();
            terms.split_whitespace().all(|t| searchable.contains(t))
        })
        .collect();

    let mut provider_sections: Vec<ProviderSection<'_>> = Vec::new();
    if show_provider_rail {
        let recent_keys = s.cfg.general.recent_models_for(agent_id);
        let recent = recent_catalog_models(recent_keys, &filtered_models, providers);
        if !recent.is_empty() {
            provider_sections.push(ProviderSection {
                provider_id: RECENT_PROVIDER_ID.to_owned(),
                title: tr_ref(lang, "Recently used").to_owned(),
                models: recent,
                recent: true,
                unavailable: false,
            });
        }
    }
    provider_sections.extend(group_models_by_provider(
        &filtered_models,
        providers,
        !searching,
    ));

    let active_provider_id: Option<String> = if show_provider_rail {
        let picked = s.model_picker.picked_provider.as_deref();
        if picked.is_some_and(|p| provider_sections.iter().any(|sec| sec.provider_id == p)) {
            picked.map(str::to_owned)
        } else {
            effective
                .and_then(|eff| {
                    provider_sections
                        .iter()
                        .find(|sec| !sec.recent && sec.provider_id == eff.provider_id)
                })
                .or_else(|| provider_sections.first())
                .map(|sec| sec.provider_id.clone())
        }
    } else {
        None
    };

    let visible_sections: Vec<&ProviderSection<'_>> = if let Some(active_pid) = &active_provider_id
    {
        provider_sections
            .iter()
            .filter(|sec| sec.provider_id == *active_pid)
            .collect()
    } else {
        provider_sections.iter().collect()
    };

    let disabled_providers: HashSet<&str> = providers
        .iter()
        .filter(|p| p.activation.as_deref() == Some("disabled"))
        .map(|p| p.id.as_str())
        .collect();
    let unavailable_providers: HashSet<&str> = providers
        .iter()
        .filter(|p| !p.available)
        .map(|p| p.id.as_str())
        .collect();

    let is_unavailable = |m: &ModelInfo| -> bool {
        !m.enabled
            || unavailable_providers.contains(m.provider_id.as_str())
            || disabled_providers.contains(m.provider_id.as_str())
    };

    // Enter selects the existing choice when search is empty, else the first enabled match.
    let first_match: Option<(String, String)> = if terms.is_empty() {
        effective
            .and_then(|eff| {
                all_models.iter().find(|m| {
                    !is_unavailable(m)
                        && m.id == eff.model_id
                        && (eff.provider_id.is_empty() || m.provider_id == eff.provider_id)
                })
            })
            .or_else(|| {
                visible_sections
                    .iter()
                    .flat_map(|sec| sec.models.iter().copied())
                    .find(|m| !is_unavailable(m))
            })
    } else {
        visible_sections
            .iter()
            .flat_map(|sec| sec.models.iter().copied())
            .find(|m| !is_unavailable(m))
            .or_else(|| filtered_models.iter().copied().find(|m| !is_unavailable(m)))
    }
    .map(|m| (m.provider_id.clone(), m.id.clone()));

    let first_option_key = visible_sections
        .iter()
        .flat_map(|sec| sec.models.iter().copied())
        .find(|m| !is_unavailable(m))
        .map(|m| format!("{key}-option-{}-{}", m.provider_id, m.id));

    let select = move |s: &mut UiState, provider: String, id: String| {
        close(s);
        on_pick(s, provider, id);
    };

    // Header: title + current model · effort subtitle
    let current_caption = effective.map(|eff| {
        let info = catalog.and_then(|view| super::labels::catalog_model_info(view, eff));
        let model_name = super::labels::format_model_name(
            eff,
            tr_ref(lang, "Model"),
            info.map(|i| i.name.as_str()),
        );
        let has_variants = info
            .and_then(|i| i.variants.as_ref())
            .is_some_and(|v| !v.is_empty());
        let effort_text = eff
            .variant
            .as_deref()
            .map(super::labels::readable_catalog_id)
            .or_else(|| {
                (has_variants || current.is_none()).then(|| tr_ref(lang, "Default").to_owned())
            });
        match effort_text {
            Some(effort) => format!("{model_name} · {effort}"),
            None => model_name,
        }
    });

    let mut header = col().w_full().gap(2.0).px(2.0).child(
        text(tr_ref(lang, "Select model"))
            .font(k.ty.ui_medium)
            .color(k.c.text_base),
    );
    if let Some(caption) = current_caption {
        header = header.child(
            text(caption)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .truncate(),
        );
    }

    let look = s.look();
    let mut body = col()
        .key(format!("{key}-panel"))
        .role("dialog")
        .label(tr_ref(lang, "Select model"))
        .w(panel_w)
        .padding(PANEL_PAD)
        .min_h(0.0)
        .shrink()
        .gap(8.0)
        .modal(true, close)
        .focus_return(anchor_key.clone())
        .on_press(|_: &mut UiState| {})
        .child(header);

    // Search field
    if !all_models.is_empty() {
        let submit = select.clone();
        body = body.child(
            TextInput::new(&s.model_query)
                .placeholder(search_placeholder)
                .label(search_placeholder)
                .leading_icon(&rune_kit::glyphs::SEARCH)
                .leading_icon_size(14.0)
                .width(Some(inner_w))
                .on_edit(|s: &mut UiState| &mut s.model_query)
                .on_key(move |_: &mut UiState, event, tx| {
                    if event.key == rune::Key::Down {
                        if let Some(opt_key) = &first_option_key {
                            tx.focus(opt_key, true);
                            return true;
                        }
                    }
                    false
                })
                .on_submit(move |s: &mut UiState, _| {
                    if let Some((p, id)) = &first_match {
                        submit(s, p.clone(), id.clone());
                    }
                })
                .build(k)
                .key("model-filter"),
        );
    }

    // All models / Free only segmented control (only when free models exist)
    if show_filter {
        body = body.child(free_filter_bar(k, lang, filter_free));
    }

    // Right column: models list for active section(s)
    let mut model_list = col()
        .w_full()
        .gap(2.0)
        .role("menu")
        .focus_group()
        .label(tr_ref(lang, "Model"));

    let mut any_row = false;
    for section in &visible_sections {
        if !show_provider_rail && !section.title.is_empty() {
            model_list = model_list.child(
                text(section.title.clone())
                    .px(10.0)
                    .py(4.0)
                    .font(k.ty.small_medium)
                    .color(k.c.text_faint),
            );
        }
        if section.unavailable && section.models.is_empty() {
            any_row = true;
            model_list = model_list.child(
                text(tr_ref(lang, "Not signed in on the host"))
                    .px(10.0)
                    .py(12.0)
                    .font(k.ty.small)
                    .color(k.c.text_muted),
            );
        }
        for &model in &section.models {
            any_row = true;
            let is_selected = effective.is_some_and(|eff| {
                eff.model_id == model.id
                    && (eff.provider_id.is_empty() || eff.provider_id == model.provider_id)
            });
            let signed_out = unavailable_providers.contains(model.provider_id.as_str());
            let unavailable = is_unavailable(model);
            let unavailable_hint = unavailable.then(|| {
                if signed_out {
                    tr_ref(lang, "Not signed in on the host").to_owned()
                } else {
                    model
                        .status
                        .clone()
                        .filter(|st| !st.trim().is_empty())
                        .unwrap_or_else(|| tr_ref(lang, "Set up on the host").to_owned())
                }
            });
            let caption = if section.recent {
                let p_name = provider_title(providers, &model.provider_id);
                match model_caption(model) {
                    Some(cap) => Some(format!("{p_name} · {cap}")),
                    None => Some(p_name),
                }
            } else {
                model_caption(model)
            };
            let (p_id, m_id, pick) = (model.provider_id.clone(), model.id.clone(), select.clone());
            model_list = model_list.child(model_row(
                k,
                lang,
                format!("{key}-option-{}-{}", model.provider_id, model.id),
                model,
                caption,
                is_selected,
                unavailable,
                unavailable_hint,
                effective.and_then(|e| e.variant.as_deref()),
                slot,
                move |s| pick(s, p_id.clone(), m_id.clone()),
            ));
        }
    }

    if !any_row {
        let empty_msg = if !terms.is_empty() {
            tr_ref(lang, "No matching models")
        } else if filter_free && !all_models.is_empty() {
            tr_ref(lang, "No free models on this host.")
        } else {
            tr_ref(lang, "No models on this host.")
        };
        model_list = model_list.child(
            text(empty_msg)
                .px(10.0)
                .py(20.0)
                .color(k.c.text_muted)
                .font(k.ty.small),
        );
    }

    let list_scroll_key = format!(
        "model-results-{}-{}-{}",
        active_provider_id.as_deref().unwrap_or("all"),
        filter_free,
        terms
    );
    let content = if show_provider_rail && !provider_sections.is_empty() {
        let mut rail_list = col().w_full().gap(2.0).role("tablist");
        for section in &provider_sections {
            let selected = active_provider_id.as_deref() == Some(section.provider_id.as_str());
            rail_list = rail_list.child(provider_rail_item(k, section, selected));
        }
        let left_rail = col()
            .key("model-provider-rail")
            .w(PROVIDER_RAIL_W)
            .max_h(LIST_MAX_H)
            .min_h(0.0)
            .shrink()
            .clip_soft(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(rail_list);
        let right_pane = col()
            .key(list_scroll_key)
            .grow(1.0)
            .min_w(0.0)
            .max_h(LIST_MAX_H)
            .min_h(0.0)
            .shrink()
            .clip_soft(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(model_list);
        let divider = el()
            .w(1.0)
            .h_full()
            .fill(k.c.border_weak.alpha(0.5))
            .a11y_hidden();
        row()
            .w_full()
            .min_h(0.0)
            .max_h(LIST_MAX_H)
            .gap(8.0)
            .child(left_rail)
            .child(divider)
            .child(right_pane)
    } else {
        // Single-column mode sits directly in `body` (`col`): do NOT set `.grow(1.0)`
        // because in a vertical column without fixed height, `.grow(1.0)` + `.min_h(0.0)`
        // + `.scroll(...)` collapses the viewport to 0 px.
        col()
            .key(list_scroll_key)
            .w_full()
            .max_h(LIST_MAX_H)
            .min_h(0.0)
            .shrink()
            .clip_soft(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(model_list)
    };

    let panel = look.floating_surface(k, body.child(content), k.radius.lg);

    layer.push(rune_kit::scrim(format!("{key}-scrim"), close));
    layer.push(
        panel
            .anchored_fit(&anchor_key, Side::Top, Align::Start, 6.0)
            .transition(Transition::opacity().combined(Transition::scale(0.97)))
            .token_motion(k.motion.overlay),
    );
    anchored_trigger
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::{AgentCatalog, CatalogDefaults, ModelVariantInfo};
    use rune::{el, Cx, Env, Key, KeyEvent, Mods, Runtime};
    use serde_json::json;

    fn sample_model(
        provider_id: &str,
        id: &str,
        name: &str,
        context: Option<u64>,
        variants: &[&str],
        cost: Option<serde_json::Value>,
    ) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: name.into(),
            provider_id: provider_id.into(),
            family: None,
            limit: context.map(|c| json!({ "context": c })),
            variants: (!variants.is_empty()).then(|| {
                variants
                    .iter()
                    .map(|v| ModelVariantInfo {
                        id: (*v).into(),
                        reasoning_effort: None,
                    })
                    .collect()
            }),
            cost,
            enabled: true,
            status: None,
        }
    }

    #[test]
    fn free_detection_uses_cost_tiers_before_name_fallback() {
        let free_by_cost = sample_model(
            "opencode",
            "beam-free",
            "Beam Free",
            Some(1_000_000),
            &[],
            Some(json!({ "input": 0, "output": 0 })),
        );
        assert!(is_free_model(&free_by_cost));

        // Even if the name contains "free", non-zero price tiers mean it is paid.
        let paid_with_free_name = sample_model(
            "opencode",
            "model-free-tier",
            "Model Free",
            None,
            &[],
            Some(json!([{ "input": 0.5, "output": 1.0 }])),
        );
        assert!(!is_free_model(&paid_with_free_name));

        // Without cost metadata, name/id fallback works.
        let name_fallback = sample_model("openrouter", "x/y:free", "Demo", None, &[], None);
        assert!(is_free_model(&name_fallback));
    }

    #[test]
    fn model_caption_formats_context_and_ordered_effort_range() {
        let m = sample_model(
            "opencode",
            "deepseek-v4.1",
            "DeepSeek V4.1 Flash",
            Some(1_000_000),
            &["max", "low", "high"],
            None,
        );
        assert_eq!(
            model_caption(&m).as_deref(),
            Some("1.0M context · low to max")
        );

        let small = sample_model("openai", "gpt-5", "GPT-5", Some(200_000), &["high"], None);
        assert_eq!(
            model_caption(&small).as_deref(),
            Some("200k context · high")
        );

        let with_none = sample_model(
            "opencode",
            "hy4-flash",
            "Hy4 flash",
            Some(1_000_000),
            &["high", "none"],
            None,
        );
        assert_eq!(
            model_caption(&with_none).as_deref(),
            Some("1.0M context · none to high")
        );
    }

    #[test]
    fn searching_identical_names_by_provider_selects_the_matching_id_and_resets_on_reopen() {
        fn view(s: &UiState, _: &mut Cx<UiState>) -> El {
            let k = Kit::neutral(true);
            let mut layer = Vec::new();
            let models = [
                ("a".into(), "same".into(), "Alpha".into()),
                ("other".into(), "same".into(), "Alpha".into()),
            ];
            let trigger = super::super::composer::model_menu(
                s,
                &k,
                &mut layer,
                OpenMenu::StartModel,
                "home",
                Slot::Start,
                "opencode",
                s.menu == Some(OpenMenu::StartModel),
                "Model".into(),
                None,
                &models,
                None,
                |s, p, id| {
                    s.start_model = Some(super::super::composer::choose_model(None, None, p, id));
                    close(s)
                },
            );
            rune_kit::root(el().w_full().h_full().center().child(trigger), layer)
        }
        let (commands, _) = crate::model::commands::channel();
        let mut s = UiState::new(commands);
        s.toggle_composer_menu(OpenMenu::StartModel);
        let mut rt = Runtime::new(s, view, (800.0, 600.0), Env::new(true, 1.0));
        for i in 0..20 {
            rt.frame(i as f64 / 60.0);
        }
        rt.key(KeyEvent {
            key: Key::Text("OTHER al".into()),
            mods: Mods::default(),
        });
        rt.frame(0.4);
        assert_eq!(rt.state.model_query.text, "OTHER al");
        assert!(rt.find("start-model-home-option-a-same").is_none());
        assert!(rt.find("start-model-home-option-other-same").is_some());
        rt.key(KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        });
        rt.frame(0.5);
        let model = rt.state.start_model.as_ref().expect("submitted result");
        assert_eq!((&*model.provider_id, &*model.model_id), ("other", "same"));
        rt.state.toggle_composer_menu(OpenMenu::StartModel);
        rt.invalidate();
        rt.frame(0.6);
        assert!(rt.find("start-model-home-option-a-same").is_some());
    }

    #[test]
    fn provider_rail_free_filter_and_inline_variant_chips_work() {
        let catalog = std::sync::Arc::new(CatalogView {
            catalog: AgentCatalog {
                models: vec![
                    sample_model(
                        "anthropic",
                        "claude-sonnet",
                        "Claude Sonnet",
                        Some(200_000),
                        &["low", "high"],
                        Some(json!({ "input": 3.0, "output": 15.0 })),
                    ),
                    sample_model(
                        "opencode",
                        "beam-free",
                        "Beam Free",
                        Some(1_000_000),
                        &["low", "max"],
                        Some(json!({ "input": 0, "output": 0 })),
                    ),
                ],
                providers: vec![
                    ProviderInfo {
                        id: "anthropic".into(),
                        name: "Anthropic".into(),
                        activation: Some("enabled".into()),
                        available: true,
                        models: Vec::new(),
                    },
                    ProviderInfo {
                        id: "opencode".into(),
                        name: "OpenCode Go".into(),
                        activation: Some("enabled".into()),
                        available: true,
                        models: Vec::new(),
                    },
                ],
                defaults: CatalogDefaults {
                    model: Some(ModelRef {
                        provider_id: "anthropic".into(),
                        model_id: "claude-sonnet".into(),
                        variant: None,
                    }),
                    mode: None,
                },
                ..Default::default()
            },
        });

        fn view(s: &UiState, _: &mut Cx<UiState>) -> El {
            let k = Kit::neutral(true);
            let mut layer = Vec::new();
            let trigger = super::super::composer::model_menu(
                s,
                &k,
                &mut layer,
                OpenMenu::StartModel,
                "home",
                Slot::Start,
                "opencode",
                s.menu == Some(OpenMenu::StartModel),
                "Claude Sonnet".into(),
                s.start_catalog.as_deref(),
                &[],
                s.start_model.as_ref(),
                |s, p, id| {
                    s.start_model = Some(super::super::composer::choose_model(
                        s.start_catalog.as_deref(),
                        s.start_model.as_ref(),
                        p,
                        id,
                    ));
                    close(s);
                },
            );
            rune_kit::root(el().w_full().h_full().center().child(trigger), layer)
        }

        let (commands, _) = crate::model::commands::channel();
        let mut s = UiState::new(commands);
        s.start_catalog = Some(catalog);
        s.toggle_composer_menu(OpenMenu::StartModel);
        let mut rt = Runtime::new(s, view, (900.0, 700.0), Env::new(true, 1.0));
        for i in 0..10 {
            rt.frame(i as f64 / 60.0);
        }

        // Provider rail and free filter exist; active provider is anthropic with inline variant chips.
        assert!(rt.find("model-provider-rail").is_some());
        assert!(rt.find("model-free-filter").is_some());
        assert!(rt.find("model-provider-anthropic").is_some());
        assert!(rt.find("model-provider-opencode").is_some());
        assert!(rt
            .find("start-model-home-option-anthropic-claude-sonnet-variant-high")
            .is_some());

        // Panel background must span the full two-column width with no right-side overflow.
        let (panel_rect, _) = rt
            .presented("start-model-home-panel")
            .expect("panel surface");
        assert!(
            (panel_rect[2] - PANEL_W_TWO_COL).abs() < 1.0,
            "expected two-column panel width {PANEL_W_TWO_COL}, got {}",
            panel_rect[2]
        );
        let (filter_rect, _) = rt.presented("model-free-filter").expect("free filter");
        assert!(
            filter_rect[0] + filter_rect[2] <= panel_rect[0] + panel_rect[2] + 0.5,
            "free filter overflows panel: filter={filter_rect:?} panel={panel_rect:?}"
        );
        let (all_rect, _) = rt.presented("model-filter-all").expect("all segment");
        let (free_rect, _) = rt.presented("model-filter-free").expect("free segment");
        assert!(
            (all_rect[2] - free_rect[2]).abs() < 2.0 && all_rect[2] > 180.0,
            "filter segments must split 50/50 cleanly: all={all_rect:?} free={free_rect:?}"
        );
        let (search_rect, _) = rt.presented("model-filter").expect("search input");
        assert!(
            search_rect[0] + search_rect[2] <= panel_rect[0] + panel_rect[2] + 0.5,
            "search input overflows panel: search={search_rect:?} panel={panel_rect:?}"
        );

        // Switching to "Free only" narrows to opencode's free model.
        rt.state.model_picker.filter_free = true;
        rt.invalidate();
        rt.frame(0.3);
        assert!(rt
            .find("start-model-home-option-anthropic-claude-sonnet")
            .is_none());
        assert!(rt
            .find("start-model-home-option-opencode-beam-free")
            .is_some());
    }

    #[test]
    fn single_provider_picker_renders_visible_rows_without_collapsing() {
        let catalog = std::sync::Arc::new(CatalogView {
            catalog: AgentCatalog {
                models: vec![
                    sample_model(
                        "openai",
                        "gpt-6-astra",
                        "GPT-6-Astra",
                        Some(400_000),
                        &["low", "medium", "high"],
                        None,
                    ),
                    sample_model(
                        "openai",
                        "gpt-6-mini",
                        "GPT-6-Mini",
                        Some(200_000),
                        &[],
                        None,
                    ),
                ],
                providers: vec![ProviderInfo {
                    id: "openai".into(),
                    name: "OpenAI".into(),
                    activation: Some("enabled".into()),
                    available: true,
                    models: Vec::new(),
                }],
                defaults: CatalogDefaults {
                    model: Some(ModelRef {
                        provider_id: "openai".into(),
                        model_id: "gpt-6-astra".into(),
                        variant: Some("medium".into()),
                    }),
                    mode: None,
                },
                ..Default::default()
            },
        });

        fn view(s: &UiState, _: &mut Cx<UiState>) -> El {
            let k = Kit::neutral(true);
            let mut layer = Vec::new();
            let trigger = super::super::composer::model_menu(
                s,
                &k,
                &mut layer,
                OpenMenu::StartModel,
                "home",
                Slot::Start,
                "codex",
                s.menu == Some(OpenMenu::StartModel),
                "GPT-6-Astra".into(),
                s.start_catalog.as_deref(),
                &[],
                s.start_model.as_ref(),
                |s, p, id| {
                    s.start_model = Some(super::super::composer::choose_model(
                        s.start_catalog.as_deref(),
                        s.start_model.as_ref(),
                        p,
                        id,
                    ));
                    close(s);
                },
            );
            rune_kit::root(el().w_full().h_full().center().child(trigger), layer)
        }

        let (commands, _) = crate::model::commands::channel();
        let mut s = UiState::new(commands);
        s.start_catalog = Some(catalog);
        s.toggle_composer_menu(OpenMenu::StartModel);
        let mut rt = Runtime::new(s, view, (900.0, 700.0), Env::new(true, 1.0));
        for i in 0..10 {
            rt.frame(i as f64 / 60.0);
        }

        assert!(rt.find("model-provider-rail").is_none());
        let (panel_rect, _) = rt
            .presented("start-model-home-panel")
            .expect("single-col panel");
        assert!((panel_rect[2] - PANEL_W_ONE_COL).abs() < 1.0);
        let (row1_rect, _) = rt
            .presented("start-model-home-option-openai-gpt-6-astra")
            .expect("first model row must be visible");
        let (row2_rect, _) = rt
            .presented("start-model-home-option-openai-gpt-6-mini")
            .expect("second model row must be visible");
        assert!(row1_rect[3] > 20.0, "row1 collapsed: {row1_rect:?}");
        assert!(row2_rect[3] > 20.0, "row2 collapsed: {row2_rect:?}");
        assert!(
            panel_rect[3] > 160.0,
            "single-col panel collapsed vertically: {panel_rect:?}"
        );
    }
}
