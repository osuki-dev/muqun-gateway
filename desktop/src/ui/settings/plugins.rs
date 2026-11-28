//! Settings → Plugins: built-in and loaded plugins, plus Bun / Herdr runtimes.

use muqun_gateway::platform::runtimes::{RuntimeKind, RuntimeSource};
use rune::{col, row as hrow, text, El};
use rune_kit::{Badge, BadgeVariant, ButtonVariant, Kit};

mod cards;
mod progress;

use super::{group, row};
use crate::plugin::packages::Operation;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

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
                .disabled(is_installing || status.is_none())
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
    let mut out = Vec::new();
    let bun = s
        .runtime_statuses
        .iter()
        .find(|st| st.kind == RuntimeKind::Bun);
    let install = tr_ref(language, "Install");
    let package_control = if bun.is_some_and(|st| st.available) {
        hrow()
            .w_full()
            .min_w(0.0)
            .gap(4.0)
            .center_y()
            .flex_wrap()
            .child(
                rune_kit::TextInput::new(&s.plugin_source)
                    .label(tr_ref(language, "Plugin source"))
                    .placeholder(tr_ref(language, "Plugin source"))
                    .width(None)
                    .on_edit(|s: &mut UiState| &mut s.plugin_source)
                    .build(k)
                    .w(220.0)
                    .min_w(140.0)
                    .grow(1.0)
                    .shrink()
                    .key("plugin-source"),
            )
            .child(
                super::button(install)
                    .disabled(s.plugin_packages.busy() || s.plugin_source.text.trim().is_empty())
                    .on_press(|s: &mut UiState| {
                        s.change_plugin_package(Operation::Install(s.plugin_source.text.clone()))
                    })
                    .build(k),
            )
            .child(
                super::button(tr_ref(language, "Choose file…"))
                    .disabled(s.plugin_packages.busy())
                    .on_press(move |_: &mut UiState| {
                        crate::platform::dialogs::pick_file(
                            tr_ref(language, "Choose plugin source"),
                            None,
                            None,
                            |path| {
                                crate::platform::dialogs::post(
                                    crate::model::inbox::UiAction::PluginFilePicked(path),
                                )
                            },
                        )
                    })
                    .build(k),
            )
    } else {
        super::button(tr_ref(language, "Install Bun"))
            .disabled(bun.is_none() || s.runtime_installing.contains(&RuntimeKind::Bun))
            .on_press(|s: &mut UiState| s.install_managed_runtime(RuntimeKind::Bun))
            .build(k)
            .key("plugin-install-bun")
    };
    let detail = match bun {
        None => "Checking for Bun…",
        Some(st) if !st.available => "Bun is required to install and run plugin packages.",
        _ => "Choose a plugin source to install.",
    };
    out.push(group(
        k,
        "plugin-packages",
        tr_ref(language, "Install plugin"),
        vec![col()
            .w_full()
            .min_w(0.0)
            .gap(8.0)
            .py(12.0)
            .child(
                text(tr_ref(language, detail))
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .w_full()
                    .wrap(),
            )
            .child(package_control)],
    ));
    if let Some(progress) = progress::install_progress(s, k) {
        out.push(progress);
    }
    out.push(cards::catalog(s, k));
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
