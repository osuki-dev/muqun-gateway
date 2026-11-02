//! The type scale and densities (architecture §6.1d). Spacing and radii are
//! CSS variables in the `/* scale */` block of `styles.css`; font sizes and
//! line heights are set on the text elements themselves, so they live here
//! with the same numbers.

pub const FS_11: f32 = 11.0;
pub const FS_12: f32 = 12.0;
pub const FS_13: f32 = 13.0;
pub const FS_15: f32 = 15.0;
pub const FS_18: f32 = 18.0;
pub const FS_24: f32 = 24.0;

/// Body text in the timeline, streaming or settled alike: the size never
/// changes when a part finishes.
pub const BODY: f32 = FS_15;
/// Paths, commands, models, tool names.
pub const MONO: f32 = FS_13;
pub const META: f32 = FS_12;

/// Row density. Comfortable is the default; compact is a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

impl Density {
    /// Body line height as a multiple of [`BODY`]: 24 px and 20 px, both on
    /// the 4 px baseline.
    pub fn line_height(self) -> f32 {
        match self {
            Density::Comfortable => 24.0 / BODY,
            Density::Compact => 20.0 / BODY,
        }
    }

    /// The class the root carries; `styles.css` scales row padding by it.
    pub fn class(self) -> &'static str {
        match self {
            Density::Comfortable => "mq-density--comfortable",
            Density::Compact => "mq-density--compact",
        }
    }

    /// `MUQUN_DENSITY=compact` until the settings page owns it.
    pub fn current() -> Self {
        match std::env::var("MUQUN_DENSITY").as_deref() {
            Ok("compact") => Density::Compact,
            _ => Density::Comfortable,
        }
    }
}
