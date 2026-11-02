//! A `.muqun-theme` manifest as a Blinc [`Theme`].
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

use super::pack::Manifest;
use super::tokens::{color_tokens, Palette};
use blinc_theme::{
    AnimationTokens, ColorScheme, ColorTokens, RadiusTokens, ShadowTokens, ShapeTokens,
    SpacingTokens, Theme, ThemeBundle, TypographyTokens,
};

impl Manifest {
    /// Read and check a `theme.json` ([`parse_manifest`]).
    #[cfg(test)]
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        super::pack::parse_manifest(json.as_bytes())
    }

    /// The palette of one variant.
    pub fn palette(&self, scheme: ColorScheme) -> anyhow::Result<Palette> {
        let variant = match scheme {
            ColorScheme::Light => &self.variants.light,
            ColorScheme::Dark => &self.variants.dark,
        };
        Palette::try_from(&variant.colors)
    }

    /// Both variants as one Blinc bundle named after the theme's id.
    pub fn bundle(&self) -> anyhow::Result<ThemeBundle> {
        let light = self.palette(ColorScheme::Light)?;
        let dark = self.palette(ColorScheme::Dark)?;
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

    /// Every colour token, by destructuring: a token Blinc adds later is a
    /// compile error here rather than an unchecked colour.
    fn all_colours(t: &ColorTokens) -> Vec<blinc_core::Color> {
        let ColorTokens {
            primary,
            primary_hover,
            primary_active,
            secondary,
            secondary_hover,
            secondary_active,
            success,
            success_bg,
            warning,
            warning_bg,
            error,
            error_bg,
            info,
            info_bg,
            background,
            surface,
            surface_elevated,
            surface_overlay,
            text_primary,
            text_secondary,
            text_tertiary,
            text_inverse,
            text_link,
            border,
            border_secondary,
            border_hover,
            border_focus,
            border_error,
            input_bg,
            input_bg_hover,
            input_bg_focus,
            input_bg_disabled,
            selection,
            selection_text,
            accent,
            accent_subtle,
            tooltip_bg,
            tooltip_text,
        } = *t;
        vec![
            primary,
            primary_hover,
            primary_active,
            secondary,
            secondary_hover,
            secondary_active,
            success,
            success_bg,
            warning,
            warning_bg,
            error,
            error_bg,
            info,
            info_bg,
            background,
            surface,
            surface_elevated,
            surface_overlay,
            text_primary,
            text_secondary,
            text_tertiary,
            text_inverse,
            text_link,
            border,
            border_secondary,
            border_hover,
            border_focus,
            border_error,
            input_bg,
            input_bg_hover,
            input_bg_focus,
            input_bg_disabled,
            selection,
            selection_text,
            accent,
            accent_subtle,
            tooltip_bg,
            tooltip_text,
        ]
    }

    #[test]
    fn every_builtin_maps_to_finite_in_range_tokens_in_both_schemes() {
        for builtin in crate::theme::store::BUILTIN {
            let pack = crate::theme::store::builtin(builtin.id).unwrap();
            let bundle = pack.manifest.bundle().unwrap();
            assert_eq!(bundle.name, builtin.id);
            assert!(bundle.css_sources.is_empty(), "no CSS rides on the bundle");
            for scheme in [ColorScheme::Light, ColorScheme::Dark] {
                let theme = bundle.for_scheme(scheme);
                assert_eq!(theme.color_scheme(), scheme);
                for c in all_colours(theme.colors()) {
                    for v in [c.r, c.g, c.b, c.a] {
                        assert!(
                            v.is_finite() && (0.0..=1.0).contains(&v),
                            "{} {scheme:?}: {c:?}",
                            builtin.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn each_scheme_takes_its_own_variant() {
        for id in ["akiba-overdrive", "jade-tempest", "scene-voyager"] {
            let manifest = &crate::theme::store::builtin(id).unwrap().manifest;
            let bundle = manifest.bundle().unwrap();
            for (scheme, variant) in [
                (ColorScheme::Light, &manifest.variants.light),
                (ColorScheme::Dark, &manifest.variants.dark),
            ] {
                let colors = bundle.for_scheme(scheme).colors().clone();
                let want = |hex: &str| blinc_core::Color::from_hex_str(hex).unwrap();
                assert_eq!(
                    colors.background,
                    want(&variant.colors.background),
                    "{id} {scheme:?}"
                );
                assert_eq!(
                    colors.primary,
                    want(&variant.colors.primary),
                    "{id} {scheme:?}"
                );
                assert_eq!(
                    colors.text_primary,
                    want(&variant.colors.text),
                    "{id} {scheme:?}"
                );
            }
            let light = bundle.for_scheme(ColorScheme::Light).colors().background;
            let dark = bundle.for_scheme(ColorScheme::Dark).colors().background;
            let luma = |c: blinc_core::Color| 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
            assert!(luma(light) > luma(dark), "{id}: light is lighter than dark");
        }
    }

    #[test]
    fn cover_courier_builds_a_bundle_with_both_schemes() {
        let file = Manifest::parse(crate::theme::COVER_COURIER_JSON).unwrap();
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
