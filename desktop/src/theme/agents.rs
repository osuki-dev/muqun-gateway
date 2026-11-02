//! The ten agents' brand colours (architecture §6.1b, §5): one fixed hue per
//! agent, the same in every theme, so a badge reads the same everywhere. Only
//! the lightness adapts to the scheme -- lighter on dark, darker on light --
//! never the hue. Together with `tokens.rs` this is the only place colour
//! literals may live (`scripts/lint-theme-tokens.sh`); views and `styles.css`
//! name them as `var(--agent-<id>)`.

use blinc_core::Color;
use blinc_theme::ColorScheme;

use super::tokens::{hsl_to_rgb, rgb_to_hsl};

/// Id, base colour, hue name. Hues: codex 160°, opencode 25°, deepseek 230°,
/// t3 260°, fx 292°, qoder 189°, pi 45°, claude 358°, antigravity 85°, cursor
/// 215° (low saturation). `other` is the neutral for an id this build does not
/// know.
const BRAND: [(&str, &str); 11] = [
    ("codex", "#10A37F"),
    ("opencode", "#F97316"),
    ("deepseek", "#4F6BFE"),
    ("t3", "#8B5CF6"),
    ("fx", "#D946EF"),
    ("qoder", "#06B6D4"),
    ("pi", "#EAB308"),
    ("claude", "#E5484D"),
    ("antigravity", "#65A30D"),
    ("cursor", "#94A3B8"),
    ("other", "#94A3B8"),
];

/// Lightness bounds: on a light scheme no brand colour is lighter than this
/// (so yellow and slate keep contrast on white), on a dark one none is darker.
const LIGHT_MAX: f32 = 0.42;
const DARK_MIN: f32 = 0.60;

/// `base` with its lightness clamped for `scheme`; hue and saturation kept.
fn adapt(base: Color, scheme: ColorScheme) -> Color {
    let (h, s, l) = rgb_to_hsl(base.r, base.g, base.b);
    let l = match scheme {
        ColorScheme::Light => l.min(LIGHT_MAX),
        ColorScheme::Dark => l.max(DARK_MIN),
    };
    let (r, g, b) = hsl_to_rgb(h, s, l);
    Color::rgba(r, g, b, 1.0)
}

fn hex(c: Color) -> String {
    let byte = |v: f32| (v * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

/// A `:root` block defining `--agent-<id>` for `scheme`, and the
/// `.mq-agent--<id>` rules that use them (text and border for a badge, fill
/// for a dot). The rules live here, not in `styles.css`, on purpose: Blinc
/// resolves `var()` when a sheet is parsed, from the variables defined before
/// it, so the rules must be in the same block as the variables they read.
/// Added once at start and again whenever the scheme changes; later rules win.
pub fn css(scheme: ColorScheme) -> String {
    let mut out = String::from(":root {\n");
    for (id, base) in BRAND {
        let base = Color::from_hex_str(base).expect("a valid brand colour");
        out.push_str(&format!(
            "    --agent-{id}: {};\n",
            hex(adapt(base, scheme))
        ));
    }
    out.push_str("}\n");
    for (id, _) in BRAND {
        out.push_str(&format!(
            ".mq-agent--{id} {{ color: var(--agent-{id}); border-color: var(--agent-{id}); }}\n"
        ));
        out.push_str(&format!(
            ".mq-agent-dot.mq-agent--{id} {{ background: var(--agent-{id}); }}\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(css: &str, id: &str) -> Color {
        let line = css
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("--agent-{id}:")))
            .unwrap();
        let value = line.split(':').nth(1).unwrap().trim().trim_end_matches(';');
        Color::from_hex_str(value).unwrap()
    }

    #[test]
    fn every_agent_has_a_variable_in_both_schemes() {
        for scheme in [ColorScheme::Light, ColorScheme::Dark] {
            let css = css(scheme);
            for (id, _) in BRAND {
                parse(&css, id);
            }
        }
    }

    #[test]
    fn only_lightness_changes_with_the_scheme_never_the_hue() {
        let (light, dark) = (css(ColorScheme::Light), css(ColorScheme::Dark));
        for (id, base) in BRAND {
            let base = Color::from_hex_str(base).unwrap();
            let (bh, bs, _) = rgb_to_hsl(base.r, base.g, base.b);
            for (css, scheme) in [(&light, ColorScheme::Light), (&dark, ColorScheme::Dark)] {
                let c = parse(css, id);
                let (h, s, l) = rgb_to_hsl(c.r, c.g, c.b);
                assert!(
                    ((h - bh) * 360.0).abs() < 2.0,
                    "{id} hue moved on {scheme:?}"
                );
                assert!((s - bs).abs() < 0.03, "{id} saturation moved");
                match scheme {
                    ColorScheme::Light => assert!(l <= LIGHT_MAX + 0.01, "{id} {l}"),
                    ColorScheme::Dark => assert!(l >= DARK_MIN - 0.01, "{id} {l}"),
                }
            }
            let (_, _, ll) = {
                let c = parse(&light, id);
                rgb_to_hsl(c.r, c.g, c.b)
            };
            let (_, _, dl) = {
                let c = parse(&dark, id);
                rgb_to_hsl(c.r, c.g, c.b)
            };
            assert!(ll < dl, "{id}: light {ll} should be darker than dark {dl}");
        }
    }

    #[test]
    fn the_generated_sheet_parses_and_styles_every_agent_class() {
        use blinc_layout::css_parser::Stylesheet;
        let css = css(ColorScheme::Dark);
        let sheet = Stylesheet::parse(&css).expect("the sheet parses");
        for (id, _) in BRAND {
            let style = sheet
                .get_class(&format!("mq-agent--{id}"))
                .unwrap_or_else(|| panic!("no rule for {id}"));
            assert!(style.text_color.is_some(), "{id} has no colour: {css}");
        }
    }
}
