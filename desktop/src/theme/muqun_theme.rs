//! A `.muqun-theme` as a Blinc [`Theme`].
//!
//! Only the colours come from the theme file (via [`super::tokens`]). The
//! rest -- typography scale, spacing, radii, shadows, motion -- is the
//! platform theme Blinc picks for this OS, so blinc_cn's components keep the
//! native proportions they were tuned for.
//!
//! No CSS rides on the bundle: `ThemeState::init` queues every CSS source a
//! bundle carries each time it is called (blinc_theme `state.rs`
//! `ThemeState::init`), so a later theme swap would stack stylesheets. The
//! app's stylesheets are added once, at startup (`crate::ui::install_css`).

use blinc_theme::{
    AnimationTokens, ColorScheme, ColorTokens, RadiusTokens, ShadowTokens, ShapeTokens,
    SpacingTokens, Theme, ThemeBundle, TypographyTokens,
};
use serde::Deserialize;

use super::tokens::{color_tokens, MuqunColors, Palette};

/// The part of a `.muqun-theme` manifest this host reads. Everything else
/// (assets, decorations, terminal palette) is ignored for now.
#[derive(Debug, Clone, Deserialize)]
pub struct ThemeFile {
    pub id: String,
    pub name: String,
    pub variants: Variants,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Variants {
    pub light: Variant,
    pub dark: Variant,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Variant {
    pub colors: MuqunColors,
}

impl ThemeFile {
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        let file: Self = serde_json::from_str(json)?;
        anyhow::ensure!(!file.id.is_empty(), "the theme has no id");
        Ok(file)
    }

    /// Both variants as one Blinc bundle named after the theme's id.
    pub fn bundle(&self) -> anyhow::Result<ThemeBundle> {
        let light = Palette::try_from(&self.variants.light.colors)?;
        let dark = Palette::try_from(&self.variants.dark.colors)?;
        let base = blinc_theme::platform_theme_bundle();
        Ok(ThemeBundle::new(
            self.id.clone(),
            MuqunTheme::new(&self.name, ColorScheme::Light, &light, &*base.light),
            MuqunTheme::new(&self.name, ColorScheme::Dark, &dark, &*base.dark),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct MuqunTheme {
    name: String,
    scheme: ColorScheme,
    colors: ColorTokens,
    typography: TypographyTokens,
    spacing: SpacingTokens,
    radii: RadiusTokens,
    shape: ShapeTokens,
    shadows: ShadowTokens,
    animations: AnimationTokens,
}

impl MuqunTheme {
    pub fn new(name: &str, scheme: ColorScheme, palette: &Palette, base: &dyn Theme) -> Self {
        let mut typography = base.typography().clone();
        typography.font_sans.name = crate::ui::text::UI_FONT.to_string();
        typography.font_mono.name = crate::ui::text::MONO_FONT.to_string();
        Self {
            name: name.to_string(),
            scheme,
            colors: color_tokens(palette, scheme),
            typography,
            spacing: base.spacing().clone(),
            radii: base.radii().clone(),
            shape: *base.shape(),
            shadows: base.shadows().clone(),
            animations: base.animations().clone(),
        }
    }
}

impl Theme for MuqunTheme {
    fn name(&self) -> &str {
        &self.name
    }

    fn color_scheme(&self) -> ColorScheme {
        self.scheme
    }

    fn colors(&self) -> &ColorTokens {
        &self.colors
    }

    fn typography(&self) -> &TypographyTokens {
        &self.typography
    }

    fn spacing(&self) -> &SpacingTokens {
        &self.spacing
    }

    fn radii(&self) -> &RadiusTokens {
        &self.radii
    }

    fn shape(&self) -> &ShapeTokens {
        &self.shape
    }

    fn shadows(&self) -> &ShadowTokens {
        &self.shadows
    }

    fn animations(&self) -> &AnimationTokens {
        &self.animations
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_courier_builds_a_bundle_with_both_schemes() {
        let file = ThemeFile::parse(crate::theme::COVER_COURIER_JSON).unwrap();
        assert_eq!(file.id, "cover-courier");
        let bundle = file.bundle().unwrap();
        assert_eq!(bundle.name, "cover-courier");
        assert!(bundle.css_sources.is_empty(), "no CSS rides on the bundle");
        let light = bundle.for_scheme(ColorScheme::Light);
        let dark = bundle.for_scheme(ColorScheme::Dark);
        assert_eq!(light.color_scheme(), ColorScheme::Light);
        assert_eq!(dark.color_scheme(), ColorScheme::Dark);
        assert_eq!(
            light.colors().background,
            blinc_core::Color::from_hex(0xF4F1EA)
        );
        assert_eq!(
            dark.colors().background,
            blinc_core::Color::from_hex(0x151C22)
        );
        assert_eq!(light.typography().font_sans.name, crate::ui::text::UI_FONT);
    }
}
