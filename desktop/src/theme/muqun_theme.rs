//! A `.muqun-theme` manifest as a rune-kit palette: only the colours come
//! from the theme file (via [`super::tokens`]); typography, radii, motion and
//! spacing are the kit's own.

use super::color_scheme::ColorScheme;
use super::pack::Manifest;
use super::tokens::ThemePalette;

impl Manifest {
    /// Read and check a `theme.json` ([`super::pack::parse_manifest`]).
    #[cfg(test)]
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        super::pack::parse_manifest(json.as_bytes())
    }

    /// The palette of one variant.
    pub fn palette(&self, scheme: ColorScheme) -> anyhow::Result<ThemePalette> {
        let variant = match scheme {
            ColorScheme::Light => &self.variants.light,
            ColorScheme::Dark => &self.variants.dark,
        };
        ThemePalette::try_from(&variant.colors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_variants_of_the_reference_theme_read() {
        let m = Manifest::parse(crate::theme::COVER_COURIER_JSON).unwrap();
        let light = m.palette(ColorScheme::Light).unwrap();
        let dark = m.palette(ColorScheme::Dark).unwrap();
        assert!(light.background.luminance() > dark.background.luminance());
    }

    #[test]
    fn every_builtin_has_two_readable_palettes() {
        for b in crate::theme::store::BUILTIN {
            let pack = crate::theme::store::builtin(b.id).expect(b.id);
            for scheme in [ColorScheme::Light, ColorScheme::Dark] {
                pack.manifest.palette(scheme).expect(b.id);
            }
        }
    }
}
