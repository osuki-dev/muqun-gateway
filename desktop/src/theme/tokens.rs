//! The muqun theme's 17 colour tokens (`app/src/theme/schema.ts`,
//! `themeColorsSchema`) mapped onto Blinc's semantic `ColorTokens`.
//!
//! One pure function, [`color_tokens`], so the mapping is a table a test can
//! read rather than something spread over the UI. Where muqun has no
//! counterpart the colour is derived:
//!
//! * `*-hover` / `*-active`: the base colour with its HSL lightness moved
//!   towards the scheme's contrast side (lighter on dark, darker on light) by
//!   [`HOVER_SHIFT`] / [`ACTIVE_SHIFT`].
//! * `*-bg` for the semantic colours: the theme's own subtle colour when it
//!   has one (`dangerSubtle`), else the colour at [`SOFT_ALPHA`].
//! * `selection`: primary at [`SELECTION_ALPHA`].

use blinc_core::Color;
use blinc_theme::{ColorScheme, ColorTokens};
use serde::Deserialize;

pub const HOVER_SHIFT: f32 = 0.06;
pub const ACTIVE_SHIFT: f32 = 0.12;
pub const SOFT_ALPHA: f32 = 0.12;
pub const SELECTION_ALPHA: f32 = 0.28;

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
pub struct Palette {
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

impl TryFrom<&MuqunColors> for Palette {
    type Error = anyhow::Error;

    fn try_from(c: &MuqunColors) -> anyhow::Result<Self> {
        let hex = |name: &str, value: &str| {
            Color::from_hex_str(value)
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

/// Every Blinc colour token for `palette` in `scheme`.
pub fn color_tokens(p: &Palette, scheme: ColorScheme) -> ColorTokens {
    let toward = |c: Color, amount: f32| shift_lightness(c, scheme, amount);
    let soft = |c: Color| c.with_alpha(SOFT_ALPHA);
    ColorTokens {
        primary: p.primary,
        primary_hover: toward(p.primary, HOVER_SHIFT),
        primary_active: toward(p.primary, ACTIVE_SHIFT),
        secondary: p.text_muted,
        secondary_hover: toward(p.text_muted, HOVER_SHIFT),
        secondary_active: toward(p.text_muted, ACTIVE_SHIFT),

        success: p.success,
        success_bg: soft(p.success),
        warning: p.warning,
        warning_bg: soft(p.warning),
        error: p.danger,
        error_bg: p.danger_subtle,
        info: p.info,
        info_bg: soft(p.info),

        background: p.background,
        surface: p.surface,
        surface_elevated: p.surface,
        surface_overlay: p.surface_raised,

        text_primary: p.text,
        text_secondary: p.text_muted,
        text_tertiary: p.text_subtle,
        text_inverse: p.on_primary,
        text_link: p.primary,

        border: p.border,
        border_secondary: p.border_strong,
        border_hover: p.border_strong,
        border_focus: p.primary,
        border_error: p.danger,

        input_bg: p.surface,
        input_bg_hover: toward(p.surface, HOVER_SHIFT / 3.0),
        input_bg_focus: p.surface,
        input_bg_disabled: p.surface_raised,

        selection: p.primary.with_alpha(SELECTION_ALPHA),
        selection_text: p.text,

        accent: p.primary,
        accent_subtle: p.primary_subtle,

        tooltip_bg: p.text,
        tooltip_text: p.background,
    }
}

/// `c` with its HSL lightness moved by `amount` towards the contrast side of
/// `scheme`: darker on a light scheme, lighter on a dark one. Alpha is kept.
pub fn shift_lightness(c: Color, scheme: ColorScheme, amount: f32) -> Color {
    let (h, s, l) = rgb_to_hsl(c.r, c.g, c.b);
    let l = match scheme {
        ColorScheme::Light => l - amount,
        ColorScheme::Dark => l + amount,
    }
    .clamp(0.0, 1.0);
    let (r, g, b) = hsl_to_rgb(h, s, l);
    Color::rgba(r, g, b, c.a)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
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

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
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
    use blinc_theme::ColorToken;

    fn hex(s: &str) -> Color {
        Color::from_hex_str(s).unwrap()
    }

    fn close(a: Color, b: Color) -> bool {
        [(a.r, b.r), (a.g, b.g), (a.b, b.b), (a.a, b.a)]
            .iter()
            .all(|(x, y)| (x - y).abs() < 0.002)
    }

    fn lightness(c: Color) -> f32 {
        rgb_to_hsl(c.r, c.g, c.b).2
    }

    fn courier() -> (Palette, Palette) {
        let theme =
            crate::theme::muqun_theme::ThemeFile::parse(crate::theme::COVER_COURIER_JSON).unwrap();
        (
            Palette::try_from(&theme.variants.light.colors).unwrap(),
            Palette::try_from(&theme.variants.dark.colors).unwrap(),
        )
    }

    #[test]
    fn hsl_round_trips() {
        for c in [
            "#A62A18", "#9FE4CE", "#151C22", "#FFFFFF", "#000000", "#808080",
        ] {
            let c = hex(c);
            let (h, s, l) = rgb_to_hsl(c.r, c.g, c.b);
            let (r, g, b) = hsl_to_rgb(h, s, l);
            assert!(close(c, Color::rgba(r, g, b, 1.0)), "{c:?}");
        }
    }

    #[test]
    fn the_direct_tokens_are_the_theme_colours() {
        let (light, _) = courier();
        let t = color_tokens(&light, ColorScheme::Light);
        let direct = [
            (ColorToken::Background, "#F4F1EA"),
            (ColorToken::Surface, "#FFFCF7"),
            (ColorToken::SurfaceOverlay, "#E8E4DB"),
            (ColorToken::Border, "#E0DAD1"),
            (ColorToken::BorderSecondary, "#C2B9AE"),
            (ColorToken::TextPrimary, "#050B12"),
            (ColorToken::TextSecondary, "#4C555C"),
            (ColorToken::TextTertiary, "#535D64"),
            (ColorToken::Primary, "#A62A18"),
            (ColorToken::TextInverse, "#FFFFFF"),
            (ColorToken::AccentSubtle, "#A62A180F"),
            (ColorToken::Error, "#9E1F14"),
            (ColorToken::ErrorBg, "#9E1F140F"),
            (ColorToken::Success, "#177A53"),
            (ColorToken::Warning, "#8A5710"),
            (ColorToken::Info, "#275CB0"),
            (ColorToken::BorderFocus, "#A62A18"),
        ];
        for (token, want) in direct {
            assert!(close(t.get(token), hex(want)), "{token:?}");
        }
    }

    #[test]
    fn hover_and_active_move_towards_contrast_in_steps() {
        let (light, dark) = courier();
        let l = color_tokens(&light, ColorScheme::Light);
        assert!(lightness(l.primary_hover) < lightness(l.primary));
        assert!(lightness(l.primary_active) < lightness(l.primary_hover));
        let d = color_tokens(&dark, ColorScheme::Dark);
        assert!(lightness(d.primary_hover) > lightness(d.primary));
        assert!(lightness(d.primary_active) > lightness(d.primary_hover));
        assert!(
            (lightness(l.primary) - lightness(l.primary_hover) - HOVER_SHIFT).abs() < 0.01,
            "the shift is the documented amount"
        );
    }

    #[test]
    fn soft_backgrounds_and_selection_are_translucent_versions_of_their_colour() {
        let (light, _) = courier();
        let t = color_tokens(&light, ColorScheme::Light);
        assert!(close(t.success_bg, hex("#177A53").with_alpha(SOFT_ALPHA)));
        assert!(close(t.warning_bg, hex("#8A5710").with_alpha(SOFT_ALPHA)));
        assert!(close(t.info_bg, hex("#275CB0").with_alpha(SOFT_ALPHA)));
        assert!(close(
            t.selection,
            hex("#A62A18").with_alpha(SELECTION_ALPHA)
        ));
        assert!(close(t.tooltip_bg, hex("#050B12")));
        assert!(close(t.tooltip_text, hex("#F4F1EA")));
    }

    #[test]
    fn shifting_keeps_alpha_and_stays_in_range() {
        let c = hex("#FFFFFF80");
        let s = shift_lightness(c, ColorScheme::Dark, 0.5);
        assert!(close(s, c), "white cannot get lighter");
        assert!((s.a - c.a).abs() < 0.002);
        let black = shift_lightness(hex("#000000"), ColorScheme::Light, 0.3);
        assert!(close(black, hex("#000000")));
    }

    #[test]
    fn a_bad_colour_names_the_token() {
        let (light, _) = courier();
        let _ = light;
        let mut raw = crate::theme::muqun_theme::ThemeFile::parse(crate::theme::COVER_COURIER_JSON)
            .unwrap()
            .variants
            .light
            .colors;
        raw.surface_raised = "teal".into();
        let err = Palette::try_from(&raw).unwrap_err().to_string();
        assert!(err.contains("surfaceRaised"), "{err}");
    }
}
