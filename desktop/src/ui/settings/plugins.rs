//! Settings → Plugins: built-in and loaded plugins, plus Bun / Herdr runtimes.

use muqun_gateway::platform::runtimes::{RuntimeKind, RuntimeSource};
use rune::{col, icon, row as hrow, text, El};
use rune_kit::{Badge, BadgeVariant, ButtonVariant, Kit};

use super::{flag_disabled, group, row, row_with};
use crate::config::ConfigEdit;
use crate::plugin::{is_always_on, is_enabled, PluginRenderKind};
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

fn surface_chip(k: &Kit, key: &str, label: &str) -> El {
    hrow()
        .key(key.to_string())
        .h(20.0)
        .px(6.0)
        .center()
        .radius(k.radius.sm)
        .fill(k.c.text_base.alpha(0.06))
        .border(1.0, k.c.border_weak)
        .child(
            text(label.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted),
        )
}

fn plugin_label(
    k: &Kit,
    language: crate::config::Language,
    plugin: &dyn crate::plugin::DesktopPlugin,
) -> El {
    let title = plugin.title(language);
    let id = plugin.id();
    let desc = plugin
        .description(language)
        .unwrap_or_else(|| plugin.category().label(language).to_string());
    let kind = plugin.render_kind();
    let kind_label = match &kind {
        PluginRenderKind::Native { kind, .. } if !kind.is_empty() && kind != "native" => {
            kind.clone()
        }
        other => tr_ref(language, other.label()).to_string(),
    };

    let mut chips = hrow().key("surfaces").gap(4.0).flex_wrap().center_y();
    for slot in plugin.surfaces() {
        chips = chips.child(surface_chip(
            k,
            &format!("surf-{id}-{slot:?}"),
            slot.label(language),
        ));
    }
    chips = chips.child(surface_chip(k, &format!("kind-{id}"), &kind_label));

    col()
        .key("label")
        .w(super::LABEL_MIN_W)
        .min_w(super::LABEL_MIN_W)
        .grow(1.0)
        .shrink()
        .gap(4.0)
        .child(
            hrow()
                .key("title")
                .gap(8.0)
                .center_y()
                .child(icon(plugin.icon(), 14.0).color(k.c.icon_muted))
                .child(
                    text(title)
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .shrink()
                        .min_w(0.0)
                        .truncate(),
                )
                .child(
                    text(id.to_string())
                        .font(k.ty.mono)
                        .color(k.c.text_faint)
                        .shrink()
                        .min_w(0.0)
                        .truncate(),
                ),
        )
        .child(
            text(desc)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .w_full(),
        )
        .child(chips)
}

fn plugin_row(s: &UiState, k: &Kit, plugin: &dyn crate::plugin::DesktopPlugin) -> El {
    let language = s.cfg.general.language;
    let id = plugin.id().to_string();
    let always = is_always_on(&id);
    let enabled = is_enabled(&id, &s.cfg.plugins_disabled);
    let toggle_id = id.clone();
    let detail = if always {
        tr_ref(language, "Always on")
    } else {
        tr_ref(language, "Enabled")
    };
    let mut controls = hrow()
        .key(format!("plugin-controls-{id}"))
        .gap(8.0)
        .center_y()
        .flex_wrap();
    for slot in plugin.surfaces().into_iter().filter(|slot| {
        matches!(
            slot,
            crate::plugin::PluginSlot::Tile
                | crate::plugin::PluginSlot::Page
                | crate::plugin::PluginSlot::Drawer
        )
    }) {
        let open_id = id.clone();
        controls = controls.child(
            super::button(slot.label(language))
                .disabled(!enabled)
                .on_press(move |s: &mut UiState| s.switch_plugin_surface(&open_id, slot))
                .build(k)
                .key(format!("open-{id}-{slot:?}")),
        );
    }
    row_with(
        k,
        &format!("plugin-{id}"),
        plugin_label(k, language, plugin),
        controls.child(flag_disabled(
            k,
            &format!("plugin-{id}"),
            enabled,
            detail,
            always,
            move |s| {
                if is_always_on(&toggle_id) {
                    return;
                }
                s.edit(ConfigEdit::PluginEnabled {
                    plugin: toggle_id.clone(),
                    on: !is_enabled(&toggle_id, &s.cfg.plugins_disabled),
                });
            },
        )),
    )
}

fn runtime_detail(
    language: crate::config::Language,
    status: &muqun_gateway::platform::runtimes::RuntimeStatus,
) -> String {
    let source = match &status.source {
        Some(RuntimeSource::System(_)) => tr_ref(language, "System"),
        Some(RuntimeSource::Managed(_)) => tr_ref(language, "Managed"),
        Some(RuntimeSource::Configured(_)) => tr_ref(language, "Configured"),
        None => tr_ref(language, "Not Installed"),
    };
    let version = status
        .version
        .as_deref()
        .map(|v| format!("v{v}"))
        .unwrap_or_else(|| tr_ref(language, "unavailable").to_string());
    let path = status
        .binary_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    if path.is_empty() {
        format!("{source} · {version}")
    } else {
        format!("{source} · {version} · {path}")
    }
}

fn runtime_row(s: &UiState, k: &Kit, kind: RuntimeKind) -> El {
    let language = s.cfg.general.language;
    let status = s.runtime_statuses.iter().find(|st| st.kind == kind);
    let is_installing = s.runtime_installing.contains(&kind);
    let (title, detail) = match status {
        Some(st) => (kind.display_name(), runtime_detail(language, st)),
        None => (
            kind.display_name(),
            tr_ref(language, "unavailable").to_string(),
        ),
    };
    let available = status.is_some_and(|st| st.available);
    let badge = match status.and_then(|st| st.source.as_ref()) {
        Some(RuntimeSource::System(_))
        | Some(RuntimeSource::Managed(_))
        | Some(RuntimeSource::Configured(_)) => BadgeVariant::Accent,
        None => BadgeVariant::Subtle,
    };
    let source_label = match status.and_then(|st| st.source.as_ref()) {
        Some(RuntimeSource::System(_)) => tr_ref(language, "System"),
        Some(RuntimeSource::Managed(_)) => tr_ref(language, "Managed"),
        Some(RuntimeSource::Configured(_)) => tr_ref(language, "Configured"),
        None => tr_ref(language, "Not Installed"),
    };
    let action_label = if is_installing {
        tr_ref(language, "Installing…")
    } else if available {
        tr_ref(language, "Update")
    } else {
        tr_ref(language, "Install")
    };

    let control = hrow()
        .key(format!("rt-ctl-{}", kind.id()))
        .gap(8.0)
        .center_y()
        .child(Badge::new(source_label).variant(badge).build(k))
        .child(
            super::button(action_label)
                .variant(if available {
                    ButtonVariant::Ghost
                } else {
                    ButtonVariant::Outline
                })
                .disabled(is_installing)
                .on_press(move |s: &mut UiState| {
                    s.install_managed_runtime(kind);
                })
                .build(k)
                .key(format!("rt-install-{}", kind.id())),
        );

    row(k, &format!("rt-{}", kind.id()), title, &detail, control)
}

pub(super) fn actions(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    vec![super::button(tr_ref(language, "Refresh"))
        .icon(&crate::ui::icons::REFRESH)
        .help(tr_ref(language, "Probe Bun and Herdr again"))
        .on_press(|s: &mut UiState| s.refresh_runtimes())
        .build(k)
        .key("plugins-refresh")]
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let mut plugins = s.plugins.all();
    plugins.sort_by_key(|plugin| {
        !matches!(plugin.category(), crate::plugin::PluginCategory::Extensions)
    });
    let mut plugin_rows = Vec::with_capacity(plugins.len());
    for plugin in &plugins {
        plugin_rows.push(plugin_row(s, k, plugin.as_ref()));
    }
    let mut out = Vec::new();
    if plugin_rows.is_empty() {
        out.push(super::empty(
            k,
            "plugins-empty",
            tr_ref(language, "No plugins"),
            tr_ref(language, "Built-in and loaded plugins appear here."),
        ));
    } else {
        out.push(group(
            k,
            "plugins-list",
            tr_ref(language, "Installed"),
            plugin_rows,
        ));
    }
    out.push(group(
        k,
        "plugins-runtimes",
        tr_ref(language, "Runtimes"),
        RuntimeKind::ALL
            .into_iter()
            .map(|kind| runtime_row(s, k, kind))
            .collect(),
    ));
    out
}
