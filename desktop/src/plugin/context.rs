//! Public Desktop presentation context shared by Bun and trusted plugin pages.
use crate::{
    config::Scheme,
    theme::{apply, color_scheme::ColorScheme},
    ui::UiState,
};
use rune::Color;
use serde::Serialize;
use std::sync::OnceLock;

#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebContext {
    locale: &'static str,
    platform: &'static str,
    desktop_version: &'static str,
    theme: Theme,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<Workspace>,
}

#[derive(Clone, PartialEq, Serialize)]
pub(crate) struct Workspace {
    pub(crate) path: String,
}

#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Theme {
    id: String,
    color_scheme: &'static str,
    preference: Scheme,
    reduced_motion: bool,
    reduced_transparency: bool,
    text_scale: f32,
    #[serde(serialize_with = "serialize_colors")]
    colors: [Color; 9],
}

fn serialize_colors<S: serde::Serializer>(
    colors: &[Color; 9],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(Some(colors.len()))?;
    for (key, color) in [
        "background",
        "surface",
        "surfaceRaised",
        "border",
        "text",
        "textMuted",
        "accent",
        "onAccent",
        "danger",
    ]
    .into_iter()
    .zip(colors)
    {
        map.serialize_entry(key, &color.hex())?;
    }
    map.end()
}

fn colors(dark: bool) -> [Color; 9] {
    let scheme = if dark {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };
    if let Some(p) = apply::palette(scheme) {
        return [
            p.background,
            p.surface,
            p.surface_raised,
            p.border,
            p.text,
            p.text_muted,
            p.primary,
            p.on_primary,
            p.danger,
        ];
    }
    static NEUTRAL: [OnceLock<[Color; 9]>; 2] = [OnceLock::new(), OnceLock::new()];
    *NEUTRAL[usize::from(dark)].get_or_init(|| {
        let c = rune_kit::Kit::neutral(dark).c;
        [
            c.bg_base,
            c.layer_01,
            c.layer_02,
            c.border_base,
            c.text_base,
            c.text_muted,
            c.bg_accent,
            c.text_inverse,
            c.danger.icon,
        ]
    })
}

pub(crate) fn web_context(s: &UiState, include_workspace: bool) -> WebContext {
    let appearance = &s.cfg.appearance;
    let dark = s.is_dark(s.system_dark.get().unwrap_or(s.dark));
    WebContext {
        locale: s.cfg.general.language.as_str(),
        platform: std::env::consts::OS,
        desktop_version: env!("CARGO_PKG_VERSION"),
        theme: Theme {
            id: apply::current_theme_id().unwrap_or_else(|| appearance.theme_id.clone()),
            color_scheme: if dark { "dark" } else { "light" },
            preference: appearance.scheme,
            reduced_motion: appearance.reduced_motion || s.system_reduced_motion.get(),
            reduced_transparency: appearance.reduced_transparency,
            text_scale: appearance.text_scale(),
            colors: colors(dark),
        },
        workspace: include_workspace.then(|| Workspace {
            path: s
                .current()
                .and_then(|i| i.directory.clone())
                .unwrap_or_else(|| s.terminal_cwd().display().to_string()),
        }),
    }
}
