//! Read-only adapter for Omarchy's active palette. No shell hooks or system writes.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use super::{apply::AppliedTheme, pack::Manifest, tokens::MuqunColors};
use crate::{
    config::Scheme,
    model::{
        inbox::{Inbox, Update},
        projection::ConfigStore,
    },
};

pub const ID: &str = "system-omarchy";

fn palette_path() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let home = dirs::home_dir()?;
    // Current Omarchy and older installations before the state-directory move.
    [
        home.join(".local/state/omarchy/current/theme/colors.toml"),
        home.join(".config/omarchy/current/theme/colors.toml"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| palette_path().is_some())
}

fn source() -> anyhow::Result<Vec<u8>> {
    let path = palette_path().ok_or_else(|| anyhow::anyhow!("Omarchy palette is unavailable"))?;
    super::store::read_bounded(&path, 64 * 1024)
}

pub fn load(requested: Instant) -> anyhow::Result<AppliedTheme> {
    from_bytes(&source()?, requested)
}

fn from_bytes(bytes: &[u8], requested: Instant) -> anyhow::Result<AppliedTheme> {
    let values: BTreeMap<String, toml::Value> = toml::from_str(std::str::from_utf8(bytes)?)?;
    for key in ["background", "foreground", "accent"] {
        anyhow::ensure!(
            values.get(key).and_then(toml::Value::as_str).is_some(),
            "incomplete Omarchy palette: {key}"
        );
    }
    let color = |name: &str, fallback: &str| -> anyhow::Result<String> {
        let value = values
            .get(name)
            .and_then(toml::Value::as_str)
            .unwrap_or(fallback);
        anyhow::ensure!(
            rune::color::hex(value).is_some(),
            "invalid Omarchy colour: {name}"
        );
        Ok(value.into())
    };
    let bg = color("background", "#171717")?;
    let fg = color("foreground", "#eeeeee")?;
    let accent = color("accent", &fg)?;
    let muted = color("dark_foreground", &fg)?;
    let selection = color("selection", &bg)?;
    let colors = MuqunColors {
        background: bg.clone(),
        surface: color("dark_background", &bg)?,
        surface_raised: color("lighter_background", &bg)?,
        border: color("muted", &muted)?,
        border_strong: accent.clone(),
        text: fg.clone(),
        text_muted: muted.clone(),
        text_subtle: muted.clone(),
        text_disabled: muted,
        primary: accent.clone(),
        on_primary: bg.clone(),
        primary_subtle: selection.clone(),
        danger: color("red", &accent)?,
        danger_subtle: selection.clone(),
        success: color("green", &accent)?,
        warning: color("yellow", &accent)?,
        info: color("blue", &accent)?,
    };
    let dark = match values.get("mode").and_then(toml::Value::as_str) {
        Some("light") => false,
        Some("dark") => true,
        _ => {
            rune::color::hex(&bg).unwrap().luminance() < rune::color::hex(&fg).unwrap().luminance()
        }
    };
    // Reuse the validated manifest shape; no bundled artwork or effects are inherited.
    let mut manifest: Manifest =
        serde_json::from_str(include_str!("../../assets/themes/cover-courier.json"))?;
    manifest.id = ID.into();
    manifest.name = "Omarchy · System".into();
    manifest.description = Some("Follows the active Omarchy palette".into());
    manifest.assets.clear();
    manifest.decoration.clear();
    manifest.variant_decorations = Default::default();
    manifest.home_identity = None;
    manifest.icons = None;
    manifest.icon_directions = None;
    manifest.desktop = None;
    manifest.preview = None;
    let names = [
        "black",
        "red",
        "green",
        "yellow",
        "blue",
        "magenta",
        "cyan",
        "white",
        "bright_black",
        "bright_red",
        "bright_green",
        "bright_yellow",
        "bright_blue",
        "bright_magenta",
        "bright_cyan",
        "bright_white",
    ];
    let mut ansi: [String; 16] = Default::default();
    for (i, name) in names.iter().enumerate() {
        let fallback = if i % 8 == 0 {
            &bg
        } else if i % 8 == 7 {
            &fg
        } else {
            &accent
        };
        ansi[i] = color(name, fallback)?;
    }
    for variant in [&mut manifest.variants.light, &mut manifest.variants.dark] {
        variant.colors = colors.clone();
        variant.surfaces = None;
        variant._effects = None;
        variant.terminal = super::pack::Terminal {
            background: bg.clone(),
            background_opacity: None,
            foreground: fg.clone(),
            cursor: accent.clone(),
            link: accent.clone(),
            selection: selection.clone(),
            ansi: ansi.clone(),
        };
    }
    manifest.palette(super::color_scheme::ColorScheme::Light)?;
    Ok(AppliedTheme {
        manifest: Arc::new(manifest),
        scheme: if dark { Scheme::Dark } else { Scheme::Light },
        images: Default::default(),
        sizes: Default::default(),
        glyphs: Default::default(),
        requested,
    })
}

/// One bounded read every two seconds, off the UI thread and only while selected.
/// Invalid/half-written files keep the last good palette until a valid update arrives.
pub fn watch(inbox: Arc<Inbox>, config: Arc<ConfigStore>) {
    if !available() {
        return;
    }
    std::thread::spawn(move || {
        let mut previous = Vec::new();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if config.get().appearance.theme_id != ID {
                previous.clear();
                continue;
            }
            let Ok(bytes) = source() else {
                continue;
            };
            if bytes == previous {
                continue;
            }
            let Ok(applied) = from_bytes(&bytes, Instant::now()) else {
                continue;
            };
            previous = bytes;
            // A user choosing another theme while we read wins over the refresh.
            if config.get().appearance.theme_id == ID {
                inbox.push(Update::Theme(Box::new(
                    super::service::ThemeUpdate::SystemRefreshed(applied),
                )));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_palette_without_inheriting_art_and_rejects_invalid_updates() {
        let source = b"mode='light'\nbackground='#f9efe0'\nforeground='#40311f'\naccent='#c53a26'\ngreen='#2f7a2a'";
        let applied = from_bytes(source, Instant::now()).unwrap();
        assert_eq!(applied.scheme, Scheme::Light);
        let palette = applied
            .manifest
            .palette(super::super::color_scheme::ColorScheme::Light)
            .unwrap();
        assert_eq!(palette.background, rune::color::hex("#f9efe0").unwrap());
        assert_eq!(palette.primary, rune::color::hex("#c53a26").unwrap());
        assert!(applied.manifest.decoration.is_empty());
        assert!(from_bytes(b"accent='not-a-colour'", Instant::now()).is_err());
        assert!(from_bytes(b"accent=", Instant::now()).is_err());
    }
}
