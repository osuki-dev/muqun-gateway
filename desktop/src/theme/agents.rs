//! The agents' brand colours (architecture §6.1b, §5): one fixed hue per
//! agent, the same in every theme, so a badge reads the same everywhere. Only
//! the lightness adapts to the scheme -- lighter on dark, darker on light --
//! never the hue. With `tokens.rs` this is the only place colour literals may
//! live (`scripts/lint-theme-tokens.sh`).

use rune::Color;

use super::tokens::{hsl_to_rgb, rgb_to_hsl};

/// Id and base colour (sRGB hex). `other` is the neutral for an id this
/// build does not know.
const BRAND: [(&str, u32); 11] = [
    ("codex", 0x10A37F),
    ("opencode", 0xF97316),
    ("deepseek", 0x4F6BFE),
    ("t3", 0x8B5CF6),
    ("fx", 0xD946EF),
    ("qoder", 0x06B6D4),
    ("pi", 0xEAB308),
    ("claude", 0xE5484D),
    ("antigravity", 0x65A30D),
    ("cursor", 0x94A3B8),
    ("other", 0x94A3B8),
];

/// Lightness bounds: on a light scheme no brand colour is lighter than this
/// (so yellow and slate keep contrast on white), on a dark one none is darker.
const LIGHT_MAX: f32 = 0.42;
const DARK_MIN: f32 = 0.60;

fn channels(base: u32) -> (f32, f32, f32) {
    let c = |shift: u32| ((base >> shift) & 0xFF) as f32 / 255.0;
    (c(16), c(8), c(0))
}

/// `base` as sRGB channels with its lightness clamped for the scheme; hue and
/// saturation kept.
fn adapt(base: u32, dark: bool) -> (f32, f32, f32) {
    let (r, g, b) = channels(base);
    let (h, s, l) = rgb_to_hsl(r, g, b);
    let l = if dark {
        l.max(DARK_MIN)
    } else {
        l.min(LIGHT_MAX)
    };
    hsl_to_rgb(h, s, l)
}

/// The brand colour of `agent` for the scheme on screen.
pub fn brand(agent: &str, dark: bool) -> Color {
    let base = BRAND
        .iter()
        .find(|(id, _)| *id == agent)
        .or_else(|| BRAND.last())
        .map_or(0x94A3B8, |(_, c)| *c);
    let (r, g, b) = adapt(base, dark);
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    rune::color::hex(&format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b)))
        .unwrap_or(Color::WHITE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_has_a_colour_in_both_schemes() {
        for (id, _) in BRAND {
            for dark in [false, true] {
                let c = brand(id, dark);
                assert!(c.a == 1.0, "{id}");
            }
        }
    }

    #[test]
    fn only_lightness_changes_with_the_scheme_never_the_hue() {
        for (id, base) in BRAND {
            let (br, bg, bb) = channels(base);
            let (bh, bs, _) = rgb_to_hsl(br, bg, bb);
            let mut lightness = [0.0f32; 2];
            for (i, dark) in [false, true].into_iter().enumerate() {
                let (r, g, b) = adapt(base, dark);
                let (h, s, l) = rgb_to_hsl(r, g, b);
                assert!(
                    ((h - bh) * 360.0).abs() < 2.0,
                    "{id} hue moved (dark: {dark})"
                );
                assert!((s - bs).abs() < 0.03, "{id} saturation moved");
                if dark {
                    assert!(l >= DARK_MIN - 0.01, "{id} {l}");
                } else {
                    assert!(l <= LIGHT_MAX + 0.01, "{id} {l}");
                }
                lightness[i] = l;
            }
            assert!(
                lightness[0] < lightness[1],
                "{id}: light {} should be darker than dark {}",
                lightness[0],
                lightness[1]
            );
        }
    }

    #[test]
    fn an_unknown_agent_gets_the_neutral() {
        assert_eq!(brand("nobody", true), brand("other", true));
    }
}
