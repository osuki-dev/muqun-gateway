//! The muqun theme's 17 colour tokens (`app/src/theme/schema.ts`,
//! `themeColorsSchema`) and how they dress a rune-kit [`Kit`].
//!
//! [`MuqunColors`] is the serde shape of `variants.<scheme>.colors` in a
//! `.muqun-theme` (hex strings, as authored). [`ThemePalette`] is the same 17
//! tokens parsed into [`rune::Color`]. [`ThemePalette::kit`] derives a kit from
//! three of them (background, text, primary) with [`Kit::from_palette`], then
//! lays the theme's own surfaces, borders, text tones and status colours over
//! the derived ones, so a theme author's choices win wherever they made one.

use rune::Color;
use rune_kit::{Kit, PaletteSeed, StateColors};
use serde::Deserialize;

/// Status backgrounds that the theme gives no subtle colour for.
pub const SOFT_ALPHA: f32 = 0.12;
/// Status borders.
pub const BORDER_ALPHA: f32 = 0.40;

/// `variants.<scheme>.colors` of a `.muqun-theme`, as hex strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MuqunColors {
    pub background: String,
    pub surface: String,
    pub surface_raised: String,
    pub border: String,
    pub border_strong: String,
    pub text: String,
    pub text_muted: String,
    pub text_subtle: String,
    pub text_disabled: String,
    pub primary: String,
    pub on_primary: String,
    pub primary_subtle: String,
    pub danger: String,
    pub danger_subtle: String,
    pub success: String,
    pub warning: String,
    pub info: String,
}

/// The same 17 tokens, parsed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemePalette {
    pub background: Color,
    pub surface: Color,
    pub surface_raised: Color,
    pub border: Color,
    pub border_strong: Color,
    pub text: Color,
    pub text_muted: Color,
    pub text_subtle: Color,
    pub text_disabled: Color,
    pub primary: Color,
    pub on_primary: Color,
    pub primary_subtle: Color,
    pub danger: Color,
    pub danger_subtle: Color,
    pub success: Color,
    pub warning: Color,
    pub info: Color,
}

impl TryFrom<&MuqunColors> for ThemePalette {
    type Error = anyhow::Error;

    fn try_from(c: &MuqunColors) -> anyhow::Result<Self> {
        let hex = |name: &str, value: &str| {
            rune::color::hex(value)
                .ok_or_else(|| anyhow::anyhow!("colour `{name}` is not a hex colour: {value}"))
        };
        Ok(Self {
            background: hex("background", &c.background)?,
            surface: hex("surface", &c.surface)?,
            surface_raised: hex("surfaceRaised", &c.surface_raised)?,
            border: hex("border", &c.border)?,
            border_strong: hex("borderStrong", &c.border_strong)?,
            text: hex("text", &c.text)?,
            text_muted: hex("textMuted", &c.text_muted)?,
            text_subtle: hex("textSubtle", &c.text_subtle)?,
            text_disabled: hex("textDisabled", &c.text_disabled)?,
            primary: hex("primary", &c.primary)?,
            on_primary: hex("onPrimary", &c.on_primary)?,
            primary_subtle: hex("primarySubtle", &c.primary_subtle)?,
            danger: hex("danger", &c.danger)?,
            danger_subtle: hex("dangerSubtle", &c.danger_subtle)?,
            success: hex("success", &c.success)?,
            warning: hex("warning", &c.warning)?,
            info: hex("info", &c.info)?,
        })
    }
}

impl ThemePalette {
    /// The three colours [`Kit::from_palette`] derives the rest from.
    pub fn seed(&self) -> PaletteSeed {
        PaletteSeed {
            bg: self.background,
            fg: self.text,
            accent: self.primary,
        }
    }

    /// A kit for this palette: derived from [`Self::seed`], with the theme's
    /// own surfaces, borders, text tones and status colours on top.
    pub fn kit(&self) -> Kit {
        let mut kit = Kit::from_palette(self.seed());
        let c = &mut kit.c;
        c.layer_01 = self.surface;
        c.layer_02 = self.surface_raised;
        c.bg_button_neutral = self.surface;
        c.surface_weak = self.surface;
        c.surface_raised = self.surface;
        c.surface_raised_base = self.surface;
        c.surface_base = self.surface_raised;
        c.border_base = self.border;
        c.border_weak = self.border;
        c.border_strong = self.border_strong;
        c.std_border = self.border_strong;
        c.border_focus = self.primary;
        c.text_muted = self.text_muted;
        c.text_weak = self.text_muted;
        c.text_faint = self.text_subtle;
        c.icon_muted = self.text_muted;
        c.icon_faint = self.text_subtle;
        c.icon_weaker = self.text_subtle;
        c.std_icon = self.text_muted;
        c.text_accent = self.primary;
        c.icon_accent = self.primary;
        c.bg_accent = self.primary;
        let state = |role: Color, bg: Option<Color>| StateColors {
            bg: bg.unwrap_or_else(|| role.alpha(SOFT_ALPHA)),
            fg: role,
            border: role.alpha(BORDER_ALPHA),
            icon: role,
        };
        c.success = state(self.success, None);
        c.warning = state(self.warning, None);
        c.danger = state(self.danger, Some(self.danger_subtle));
        c.info = state(self.info, None);
        kit
    }
}

/// `(h, s, l)` in 0..=1 of an sRGB colour given as 0..=1 channels.
pub(super) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d.abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

pub(super) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s.abs() < f32::EPSILON {
        return (l, l, l);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let channel = |t: f32| {
        let t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Color {
        rune::color::hex(s).unwrap()
    }

    fn close(a: Color, b: Color) -> bool {
        [(a.r, b.r), (a.g, b.g), (a.b, b.b), (a.a, b.a)]
            .iter()
            .all(|(x, y)| (x - y).abs() < 0.002)
    }

    fn courier() -> (ThemePalette, ThemePalette) {
        let theme = crate::theme::pack::Manifest::parse(crate::theme::COVER_COURIER_JSON).unwrap();
        (
            ThemePalette::try_from(&theme.variants.light.colors).unwrap(),
            ThemePalette::try_from(&theme.variants.dark.colors).unwrap(),
        )
    }

    #[test]
    fn hsl_round_trips() {
        for c in [
            (0.65, 0.16, 0.09),
            (0.62, 0.89, 0.81),
            (0.08, 0.11, 0.13),
            (1.0, 1.0, 1.0),
            (0.0, 0.0, 0.0),
            (0.5, 0.5, 0.5),
        ] {
            let (h, s, l) = rgb_to_hsl(c.0, c.1, c.2);
            let (r, g, b) = hsl_to_rgb(h, s, l);
            assert!(
                (r - c.0).abs() < 0.002 && (g - c.1).abs() < 0.002 && (b - c.2).abs() < 0.002,
                "{c:?}"
            );
        }
    }

    #[test]
    fn the_palette_is_the_theme_colours() {
        let (light, _) = courier();
        assert!(close(light.background, hex("#F4F1EA")));
        assert!(close(light.surface, hex("#FFFCF7")));
        assert!(close(light.surface_raised, hex("#E8E4DB")));
        assert!(close(light.border, hex("#E0DAD1")));
        assert!(close(light.text, hex("#050B12")));
        assert!(close(light.primary, hex("#A62A18")));
        assert!(close(light.primary_subtle, hex("#A62A180F")));
        assert!(close(light.danger_subtle, hex("#9E1F140F")));
    }

    #[test]
    fn the_kit_wears_the_theme() {
        let (light, dark) = courier();
        let k = light.kit();
        assert!(!k.dark);
        assert!(close(k.c.layer_01, light.surface));
        assert!(close(k.c.border_strong, light.border_strong));
        assert!(close(k.c.text_muted, light.text_muted));
        assert!(close(k.c.danger.fg, light.danger));
        assert!(close(k.c.danger.bg, light.danger_subtle));
        assert!(close(k.c.success.icon, light.success));
        assert!(dark.kit().dark, "a light-on-dark palette makes a dark kit");
    }

    #[test]
    fn a_bad_colour_names_the_token() {
        let mut raw = crate::theme::pack::Manifest::parse(crate::theme::COVER_COURIER_JSON)
            .unwrap()
            .variants
            .light
            .colors;
        raw.surface_raised = "teal".into();
        let err = ThemePalette::try_from(&raw).unwrap_err().to_string();
        assert!(err.contains("surfaceRaised"), "{err}");
    }
}
