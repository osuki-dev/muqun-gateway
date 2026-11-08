//! `.muqun-theme` packages: the manifest (`theme.json`) and the ZIP around it.
//!
//! A port of the App's reader, check for check, so a pack one of them accepts
//! the other accepts too: `app/src/theme/schema.ts` (`parseThemeManifest`,
//! [`parse_manifest`] here) and `app/src/theme/package.ts`
//! (`unpackTheme`, [`unpack`] here). Same limits, same structural ZIP checks
//! (central directory against local headers, no gaps, no links, no
//! encryption, no undeclared files), same refusal messages where they exist.
//!
//! Two deliberate differences:
//!
//! * `schemaVersion` 2 is read: it adds the optional desktop block of
//!   architecture §5 ([`DesktopBlock`]). The App refuses anything above 1 with
//!   "needs a newer version of Muqun"; this reader says the same above 2.
//! * Image checks run here at unpack (`image-inspection.ts` runs them at
//!   install in the App): a declared `sha256` must match, the bytes must be
//!   the format the path's extension names, and stay under 16 M pixels.
//!
//! Only colour/terminal/asset structure is typed strictly, as in the App.
//! The App-only blocks this host does not draw (`effects`, `materials`,
//! `icons`, `homePresentation` …) are skipped, so they cannot fail a pack
//! here that the App would accept, but nor are they re-validated.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer};

use super::tokens::MuqunColors;

/// `THEME_LIMITS` in `schema.ts`.
pub const MANIFEST_BYTES: usize = 256 * 1024;
pub const MAX_ASSETS: usize = 32;
pub const ASSET_BYTES: u64 = 8 * 1024 * 1024;
pub const PACKAGE_BYTES: u64 = 25 * 1024 * 1024;
pub const EXTRACTED_BYTES: u64 = 50 * 1024 * 1024;
pub const IMAGE_PIXELS: u64 = 16_000_000;

/// The highest `schemaVersion` this build reads (App: 1; 2 adds `desktop`).
pub const SUPPORTED_SCHEMA_VERSION: u64 = 2;

/// One problem with a manifest, located by its JSON path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

/// A manifest the App would refuse, with up to 20 reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTheme {
    pub issues: Vec<Issue>,
}

impl InvalidTheme {
    fn one(path: &str, message: impl Into<String>) -> Self {
        Self {
            issues: vec![Issue {
                path: path.into(),
                message: message.into(),
            }],
        }
    }
}

impl fmt::Display for InvalidTheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lines: Vec<String> = self
            .issues
            .iter()
            .map(|i| format!("{}: {}", i.path, i.message))
            .collect();
        f.write_str(&lines.join("\n"))
    }
}

impl std::error::Error for InvalidTheme {}

/// A package the App would refuse, as `package.ts` words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPackage(pub String);

impl fmt::Display for InvalidPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid theme package: {}", self.0)
    }
}

impl std::error::Error for InvalidPackage {}

fn invalid<T>(message: &str) -> anyhow::Result<T> {
    Err(InvalidPackage(message.to_string()).into())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: String,
    pub schema_version: u64,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub min_app_version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub preview: Option<String>,
    pub variants: Variants,
    #[serde(default)]
    pub assets: BTreeMap<String, Asset>,
    #[serde(default)]
    pub decoration: BTreeMap<String, Option<Slot>>,
    #[serde(default)]
    pub variant_decorations: VariantDecorations,
    #[serde(default)]
    pub home_identity: Option<HomeIdentity>,
    #[serde(default, deserialize_with = "lenient")]
    pub home_presentation: Option<HomePresentation>,
    /// Chrome glyphs the pack replaces (`chrome.settings`, `home.arrow` …);
    /// a malformed entry is dropped, never fatal, as in the App.
    #[serde(default, deserialize_with = "lenient")]
    pub icons: Option<BTreeMap<String, Option<IconRef>>>,
    /// Which way a directional glyph (`home.arrow`) points as drawn.
    #[serde(default, deserialize_with = "lenient")]
    pub icon_directions: Option<BTreeMap<String, String>>,
    /// Architecture §5, `schemaVersion` 2. Read leniently: a value this build
    /// does not understand is dropped, never fatal. Parsed and checked now;
    /// chrome, density and typography consume it as those settings land.
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "lenient_desktop")]
    pub desktop: Option<DesktopBlock>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variants {
    pub light: Variant,
    pub dark: Variant,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub colors: MuqunColors,
    #[serde(default)]
    pub surfaces: Option<Surfaces>,
    /// Ambient effects: an App feature this host does not draw.
    #[serde(default, rename = "effects")]
    pub _effects: Option<serde_json::Value>,
    pub terminal: Terminal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Surfaces {
    #[serde(default)]
    pub background_opacity: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Terminal {
    pub background: String,
    #[serde(default)]
    pub background_opacity: Option<f64>,
    pub foreground: String,
    pub cursor: String,
    pub link: String,
    pub selection: String,
    pub ansi: [String; 16],
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

/// A decoration slot: the image, its per-width replacements, and how the
/// author wants it shown (`fit`, `opacity`, `focalPoint`; outset and the
/// rest belong to the App).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Slot {
    pub asset: String,
    #[serde(default)]
    pub compact: Option<ImageRef>,
    #[serde(default)]
    pub regular: Option<ImageRef>,
    /// `cover` or `contain` (the default).
    #[serde(default)]
    pub fit: Option<String>,
    /// 0..=1; absent means 1.
    #[serde(default)]
    pub opacity: Option<f32>,
    #[serde(default)]
    pub focal_point: Option<FocalPoint>,
}

/// Where the image's subject sits, in 0..=1 of its width and height.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct FocalPoint {
    pub x: f32,
    pub y: f32,
}

/// How a slot's image is placed, as the manifest asks for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotLook {
    pub cover: bool,
    pub opacity: f32,
    pub focal: [f32; 2],
}

impl Default for SlotLook {
    fn default() -> Self {
        Self {
            cover: false,
            opacity: 1.0,
            focal: [0.5, 0.5],
        }
    }
}

impl Slot {
    /// The image a wide window shows: the `regular` replacement when the
    /// author made one (a desktop window is the App's regular width class).
    pub fn asset(&self) -> &str {
        self.regular.as_ref().map_or(&self.asset, |r| &r.asset)
    }

    /// How the wide window shows it: the `regular` replacement's own fit,
    /// opacity and focal point, each falling back to the slot's.
    pub fn look(&self) -> SlotLook {
        let r = self.regular.as_ref();
        let fit = r.and_then(|r| r.fit.as_deref()).or(self.fit.as_deref());
        SlotLook {
            cover: fit == Some("cover"),
            opacity: r
                .and_then(|r| r.opacity)
                .or(self.opacity)
                .filter(|o| o.is_finite())
                .map_or(1.0, |o| o.clamp(0.0, 1.0)),
            focal: r
                .and_then(|r| r.focal_point)
                .or(self.focal_point)
                .map_or([0.5, 0.5], |f| [f.x.clamp(0.0, 1.0), f.y.clamp(0.0, 1.0)]),
        }
    }
}

/// A slot's per-width replacement: its own image, and optionally its own
/// placement.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRef {
    pub asset: String,
    #[serde(default)]
    pub fit: Option<String>,
    #[serde(default)]
    pub opacity: Option<f32>,
    #[serde(default)]
    pub focal_point: Option<FocalPoint>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantDecorations {
    #[serde(default)]
    pub light: BTreeMap<String, Option<Slot>>,
    #[serde(default)]
    pub dark: BTreeMap<String, Option<Slot>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HomeIdentity {
    #[serde(default)]
    pub name: Option<Mode>,
    #[serde(default)]
    pub logo: Option<Mode>,
    #[serde(default)]
    pub artwork: Option<Mode>,
}

/// `{ mode: default | hidden | custom }`, with the custom `asset` (a logo)
/// or `text` (a name).
#[derive(Debug, Clone, Deserialize)]
pub struct Mode {
    pub mode: String,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

/// How Home presents the artwork: `cover` (a tall band the greeting sits
/// in) or `standard` (a compact header).
#[derive(Debug, Clone, Deserialize)]
pub struct HomePresentation {
    pub header: String,
}

/// A replacement glyph: an asset drawn as a mask in the theme's colour
/// (`template`, the default) or as it is (`original`).
#[derive(Debug, Clone, Deserialize)]
pub struct IconRef {
    pub asset: String,
    #[serde(default)]
    pub render: Option<String>,
}

/// A direction word (`up`, `up-right` … `up-left`) as radians clockwise
/// from pointing right, the way screen rotation turns.
pub fn direction_angle(word: &str) -> Option<f32> {
    let steps = match word {
        "right" => 0.0,
        "down-right" => 1.0,
        "down" => 2.0,
        "down-left" => 3.0,
        "left" => 4.0,
        "up-left" => 5.0,
        "up" => 6.0,
        "up-right" => 7.0,
        _ => return None,
    };
    Some(steps * std::f32::consts::FRAC_PI_4)
}

impl IconRef {
    pub fn template(&self) -> bool {
        self.render.as_deref() != Some("original")
    }
}

/// Reads an optional App-only block, dropping it when it does not parse.
fn lenient<'de, D: Deserializer<'de>, T: serde::de::DeserializeOwned>(
    d: D,
) -> Result<Option<T>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).ok())
}

/// The optional desktop block (architecture §5). Every field is optional;
/// values outside the documented set or range are dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesktopBlock {
    pub titlebar: Option<Titlebar>,
    pub background_opacity: Option<f32>,
    pub glass: Option<bool>,
    pub blur: Option<f32>,
    pub shadows: Option<Shadows>,
    pub motion: Option<Motion>,
    pub ui_font: Option<String>,
    pub mono_font: Option<String>,
    pub scale: Option<f32>,
    pub density: Option<Density>,
    pub radius: Option<String>,
    pub cover: Option<CoverUse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Titlebar {
    Unified,
    Native,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shadows {
    Soft,
    Flat,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Full,
    Reduced,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Comfortable,
    Compact,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverUse {
    Sidebar,
    None,
}

fn lenient_desktop<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DesktopBlock>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(DesktopBlock::from_value(&value))
}

impl DesktopBlock {
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        let block = value.as_object()?;
        let get = |section: &str, key: &str| block.get(section).and_then(|s| s.get(key));
        let text = |v: Option<&serde_json::Value>, max: usize| {
            v.and_then(|v| v.as_str())
                .filter(|s| !s.is_empty() && s.len() <= max && plain(s))
                .map(str::to_string)
        };
        let number = |v: Option<&serde_json::Value>, min: f64, max: f64| {
            v.and_then(|v| v.as_f64())
                .filter(|n| n.is_finite() && (min..=max).contains(n))
                .map(|n| n as f32)
        };
        fn word(v: Option<&serde_json::Value>) -> Option<&str> {
            v.and_then(|v| v.as_str())
        }
        Some(Self {
            titlebar: match word(get("chrome", "titlebar")) {
                Some("unified") => Some(Titlebar::Unified),
                Some("native") => Some(Titlebar::Native),
                _ => None,
            },
            background_opacity: number(get("chrome", "backgroundOpacity"), 0.0, 1.0),
            glass: get("effects", "glass").and_then(|v| v.as_bool()),
            blur: number(get("effects", "blur"), 0.0, 64.0),
            shadows: match word(get("effects", "shadows")) {
                Some("soft") => Some(Shadows::Soft),
                Some("flat") => Some(Shadows::Flat),
                _ => None,
            },
            motion: match word(get("effects", "motion")) {
                Some("full") => Some(Motion::Full),
                Some("reduced") => Some(Motion::Reduced),
                _ => None,
            },
            ui_font: text(get("typography", "ui"), 64),
            mono_font: text(get("typography", "mono"), 64),
            scale: number(get("typography", "scale"), 0.75, 1.5),
            density: match word(block.get("density")) {
                Some("comfortable") => Some(Density::Comfortable),
                Some("compact") => Some(Density::Compact),
                _ => None,
            },
            radius: word(block.get("radius"))
                .filter(|r| matches!(*r, "none" | "sm" | "md" | "lg" | "xl"))
                .map(str::to_string),
            cover: match word(get("cover", "use")) {
                Some("sidebar") => Some(CoverUse::Sidebar),
                Some("none") => Some(CoverUse::None),
                _ => None,
            },
        })
    }
}

// ---- field formats (schema.ts) ---------------------------------------------

fn push(issues: &mut Vec<Issue>, path: &str, message: &str) {
    issues.push(Issue {
        path: path.to_string(),
        message: message.to_string(),
    });
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `#RRGGBB`.
pub fn is_opaque_colour(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && is_hex(&s[1..])
}

/// `#RRGGBB` or `#RRGGBBAA`.
pub fn is_alpha_colour(s: &str) -> bool {
    (s.len() == 7 || s.len() == 9) && s.starts_with('#') && is_hex(&s[1..])
}

/// `^[a-z][a-z0-9-]*$`, 1..=64.
pub fn is_identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    s.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `^[^\x00-\x1f\x7f<>]+$`.
fn plain(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| c < ' ' || c == '\x7f' || c == '<' || c == '>')
}

fn plain_text(s: &str, max: usize) -> bool {
    plain(s) && s.chars().count() <= max
}

/// `^\d+\.\d+\.\d+$`, at most 32.
fn is_semver(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    s.len() <= 32
        && parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `^https:\/\/[^\s]+$`, at most 2048.
fn is_https_url(s: &str) -> bool {
    s.len() <= 2048
        && s.strip_prefix("https://")
            .is_some_and(|rest| !rest.is_empty() && !rest.chars().any(char::is_whitespace))
}

/// `^assets\/[a-zA-Z0-9_-]+\.(png|jpg|jpeg|webp)$`.
pub fn is_asset_path(s: &str) -> bool {
    let Some(file) = s.strip_prefix("assets/") else {
        return false;
    };
    let Some((stem, ext)) = file.rsplit_once('.') else {
        return false;
    };
    s.len() <= 160
        && !stem.is_empty()
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && matches!(ext, "png" | "jpg" | "jpeg" | "webp")
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Read and check a `theme.json`, as `parseThemeManifest` does.
pub fn parse_manifest(bytes: &[u8]) -> anyhow::Result<Manifest> {
    if bytes.len() > MANIFEST_BYTES {
        return Err(InvalidTheme::one("$", "Theme manifest exceeds 256 KiB").into());
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| InvalidTheme::one("$", "Expected a complete JSON theme manifest"))?;
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| InvalidTheme::one("$", "Expected a complete JSON theme manifest"))?;
    // Asked before the structure, so a pack from a later format is told what
    // is actually wrong with it.
    if let Some(version) = value.get("schemaVersion").and_then(|v| v.as_f64()) {
        if version > SUPPORTED_SCHEMA_VERSION as f64 {
            return Err(InvalidTheme::one(
                "schemaVersion",
                format!(
                    "This theme needs a newer version of Muqun (it uses theme format {version}, \
                     this app reads {SUPPORTED_SCHEMA_VERSION})"
                ),
            )
            .into());
        }
    }
    let manifest: Manifest =
        serde_json::from_value(value).map_err(|err| InvalidTheme::one("$", format!("{err}")))?;
    manifest.validate()?;
    Ok(manifest)
}

impl Manifest {
    /// Every format and reference rule of `themeManifestSchema` and
    /// `parseThemeManifest` that the structure alone does not enforce.
    pub fn validate(&self) -> Result<(), InvalidTheme> {
        let mut issues = Vec::new();
        let issue = &mut |path: &str, message: &str| push(&mut issues, path, message);
        if self.format != "muqun-theme" {
            issue("format", "Expected \"muqun-theme\"");
        }
        if self.schema_version < 1 {
            issue("schemaVersion", "Expected an integer of at least 1");
        }
        if !is_identifier(&self.id) {
            issue("id", "Expected a lowercase identifier");
        }
        if !plain_text(&self.name, 64) {
            issue("name", "Expected plain text of at most 64 characters");
        }
        if !is_semver(&self.version) {
            issue("version", "Expected a version like 1.0.0");
        }
        for (path, value, max) in [
            ("author", &self.author, 100),
            ("license", &self.license, 100),
            ("description", &self.description, 280),
        ] {
            if value.as_deref().is_some_and(|v| !plain_text(v, max)) {
                issue(
                    path,
                    &format!("Expected plain text of at most {max} characters"),
                );
            }
        }
        if self.source.as_deref().is_some_and(|s| !is_https_url(s)) {
            issue("source", "Expected an https:// address");
        }
        if self
            .min_app_version
            .as_deref()
            .is_some_and(|s| !is_semver(s))
        {
            issue("minAppVersion", "Expected a version like 1.0.0");
        }
        if self.tags.len() > 12 {
            issue("tags", "At most 12 tags are allowed");
        }
        for (i, tag) in self.tags.iter().enumerate() {
            if !is_identifier(tag) {
                issue(&format!("tags.{i}"), "Expected a lowercase identifier");
            }
        }
        if self.preview.as_deref().is_some_and(|p| !is_identifier(p)) {
            issue("preview", "Expected a lowercase identifier");
        }
        for (scheme, variant) in [
            ("light", &self.variants.light),
            ("dark", &self.variants.dark),
        ] {
            variant.check(&format!("variants.{scheme}"), issue);
        }

        if self.assets.len() > MAX_ASSETS {
            issue("assets", "At most 32 assets are allowed");
        }
        for (id, asset) in &self.assets {
            let path = format!("assets.{id}");
            if !is_identifier(id) {
                issue(&path, "Expected a lowercase identifier");
            }
            match (&asset.path, &asset.url) {
                (Some(p), None) if is_asset_path(p) => {}
                (None, Some(u)) if is_https_url(u) => {}
                (Some(_), None) => issue(
                    &format!("{path}.path"),
                    "Expected assets/<name>.png|jpg|jpeg|webp",
                ),
                (None, Some(_)) => issue(&format!("{path}.url"), "Expected an https:// address"),
                _ => issue(&path, "Expected exactly one of path or url"),
            }
            if asset.sha256.as_deref().is_some_and(|s| !is_sha256(s)) {
                issue(
                    &format!("{path}.sha256"),
                    "Expected a lowercase SHA-256 hex digest",
                );
            }
        }

        let assets = &self.assets;
        let mut reference = |asset: &str, path: String| {
            if !assets.contains_key(asset) {
                issue(&path, &format!("Unknown asset: {asset}"));
            }
        };
        for (base, slots) in [
            ("decoration", &self.decoration),
            ("variantDecorations.light", &self.variant_decorations.light),
            ("variantDecorations.dark", &self.variant_decorations.dark),
        ] {
            for (key, slot) in slots {
                let Some(slot) = slot else { continue };
                reference(&slot.asset, format!("{base}.{key}.asset"));
                for (width, image) in [("compact", &slot.compact), ("regular", &slot.regular)] {
                    if let Some(image) = image {
                        reference(&image.asset, format!("{base}.{key}.{width}.asset"));
                    }
                }
            }
        }
        if let Some(logo) = self.home_identity.as_ref().and_then(|h| h.logo.as_ref()) {
            if logo.mode == "custom" {
                // `{ mode: 'custom', asset }`: the asset is required.
                reference(
                    logo.asset.as_deref().unwrap_or(""),
                    "homeIdentity.logo.asset".into(),
                );
            }
        }
        if let Some(preview) = &self.preview {
            reference(preview, "preview".into());
        }

        if issues.is_empty() {
            Ok(())
        } else {
            issues.truncate(20);
            Err(InvalidTheme { issues })
        }
    }

    /// The image a slot shows in `scheme`: the scheme's own decoration, else
    /// the shared one. `None` when the slot is absent or explicitly empty.
    pub fn slot_asset(&self, slot: &str, dark: bool) -> Option<&str> {
        let own = if dark {
            &self.variant_decorations.dark
        } else {
            &self.variant_decorations.light
        };
        match own.get(slot) {
            Some(Some(s)) => Some(s.asset()),
            Some(None) => None,
            None => self
                .decoration
                .get(slot)
                .and_then(|s| s.as_ref())
                .map(Slot::asset),
        }
    }

    /// How the slot's image is shown in `scheme` (fit, opacity, focal point),
    /// from the scheme's own decoration, else the shared one.
    pub fn slot_look(&self, slot: &str, dark: bool) -> Option<SlotLook> {
        let own = if dark {
            &self.variant_decorations.dark
        } else {
            &self.variant_decorations.light
        };
        match own.get(slot) {
            Some(Some(s)) => Some(s.look()),
            Some(None) => None,
            None => self
                .decoration
                .get(slot)
                .and_then(|s| s.as_ref())
                .map(Slot::look),
        }
    }

    /// The Home hero for `scheme`: the pack's `home.artwork` (unless its
    /// author turned it off with `homeIdentity.artwork: hidden`), else its
    /// `home.wallpaper`, else its gallery preview.
    #[allow(dead_code)]
    pub fn cover_asset(&self, dark: bool) -> Option<&str> {
        let artwork_hidden = self
            .home_identity
            .as_ref()
            .and_then(|h| h.artwork.as_ref())
            .is_some_and(|m| m.mode == "hidden");
        let artwork = (!artwork_hidden)
            .then(|| self.slot_asset("home.artwork", dark))
            .flatten();
        artwork
            .or_else(|| self.slot_asset("home.wallpaper", dark))
            .or(self.preview.as_deref())
    }
}

impl Variant {
    fn check(&self, base: &str, issue: &mut dyn FnMut(&str, &str)) {
        let c = &self.colors;
        let opaque = [
            ("background", &c.background),
            ("surface", &c.surface),
            ("surfaceRaised", &c.surface_raised),
            ("border", &c.border),
            ("borderStrong", &c.border_strong),
            ("text", &c.text),
            ("textMuted", &c.text_muted),
            ("textSubtle", &c.text_subtle),
            ("textDisabled", &c.text_disabled),
            ("primary", &c.primary),
            ("onPrimary", &c.on_primary),
            ("danger", &c.danger),
            ("success", &c.success),
            ("warning", &c.warning),
            ("info", &c.info),
        ];
        for (name, value) in opaque {
            if !is_opaque_colour(value) {
                issue(
                    &format!("{base}.colors.{name}"),
                    "Expected an opaque #RRGGBB color",
                );
            }
        }
        for (name, value) in [
            ("primarySubtle", &c.primary_subtle),
            ("dangerSubtle", &c.danger_subtle),
        ] {
            if !is_alpha_colour(value) {
                issue(&format!("{base}.colors.{name}"), "Expected a hex color");
            }
        }
        let t = &self.terminal;
        for (name, value) in [
            ("background", &t.background),
            ("foreground", &t.foreground),
            ("cursor", &t.cursor),
            ("link", &t.link),
        ] {
            if !is_opaque_colour(value) {
                issue(
                    &format!("{base}.terminal.{name}"),
                    "Expected an opaque #RRGGBB color",
                );
            }
        }
        if !is_alpha_colour(&t.selection) {
            issue(
                &format!("{base}.terminal.selection"),
                "Expected a hex color",
            );
        }
        for (i, value) in t.ansi.iter().enumerate() {
            if !is_opaque_colour(value) {
                issue(
                    &format!("{base}.terminal.ansi.{i}"),
                    "Expected an opaque #RRGGBB color",
                );
            }
        }
        let unit = |v: Option<f64>| v.is_some_and(|v| !(0.0..=1.0).contains(&v));
        if unit(t.background_opacity) {
            issue(
                &format!("{base}.terminal.backgroundOpacity"),
                "Expected a number from 0 to 1",
            );
        }
        if unit(self.surfaces.as_ref().and_then(|s| s.background_opacity)) {
            issue(
                &format!("{base}.surfaces.backgroundOpacity"),
                "Expected a number from 0 to 1",
            );
        }
    }
}

// ---- the package --------------------------------------------------------------

/// An unpacked, checked `.muqun-theme`.
#[derive(Debug, Clone)]
pub struct ThemePack {
    pub manifest: Manifest,
    /// `theme.json` exactly as packed, so an install keeps the author's file.
    pub manifest_json: Arc<[u8]>,
    /// Image bytes by asset id (the manifest's `assets` keys).
    pub assets: BTreeMap<String, Arc<[u8]>>,
}

impl ThemePack {
    /// The bytes behind asset `id`.
    pub fn asset(&self, id: &str) -> Option<Arc<[u8]>> {
        self.assets.get(id).cloned()
    }
}

struct Entry {
    name: String,
    size: u32,
    compressed: u32,
    method: u16,
    crc: u32,
    offset: u32,
    flags: u16,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

const EOCD: u32 = 0x0605_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const LOCAL: u32 = 0x0403_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;

fn allowed_name(name: &str) -> bool {
    name == "theme.json" || name == "assets/" || is_asset_path(name)
}

/// Directory metadata, checked before anything is decoded (`entries()`).
fn entries(bytes: &[u8]) -> anyhow::Result<(Vec<Entry>, usize)> {
    if bytes.len() as u64 > PACKAGE_BYTES {
        return invalid("package exceeds 25 MiB");
    }
    if bytes.len() < 22 {
        return invalid("truncated ZIP");
    }
    let floor = bytes.len().saturating_sub(65557);
    let mut end = bytes.len() - 22;
    while end > floor && u32_at(bytes, end) != EOCD {
        end -= 1;
    }
    if u32_at(bytes, end) != EOCD || end + 22 + u16_at(bytes, end + 20) as usize != bytes.len() {
        return invalid("missing ZIP directory");
    }
    let count = u16_at(bytes, end + 10) as usize;
    let directory = u32_at(bytes, end + 16) as usize;
    if u16_at(bytes, end + 4) != 0
        || u16_at(bytes, end + 6) != 0
        || u16_at(bytes, end + 8) as usize != count
        || count > MAX_ASSETS + 2
        || directory + u32_at(bytes, end + 12) as usize != end
    {
        return invalid("unsupported ZIP directory");
    }
    let mut result = Vec::with_capacity(count);
    let mut names = BTreeSet::new();
    let mut offset = directory;
    let mut expanded: u64 = 0;
    for _ in 0..count {
        if offset + 46 > end || u32_at(bytes, offset) != CENTRAL {
            return invalid("truncated ZIP entry");
        }
        let name_len = u16_at(bytes, offset + 28) as usize;
        let next = offset
            + 46
            + name_len
            + u16_at(bytes, offset + 30) as usize
            + u16_at(bytes, offset + 32) as usize;
        if next > end || u16_at(bytes, offset + 34) != 0 {
            return invalid("invalid ZIP entry length");
        }
        let Ok(name) = std::str::from_utf8(&bytes[offset + 46..offset + 46 + name_len]) else {
            return invalid("unsupported or unsafe path");
        };
        if !allowed_name(name) {
            return invalid("unsupported or unsafe path");
        }
        if !names.insert(name.to_lowercase()) {
            return invalid("duplicate path");
        }
        let flags = u16_at(bytes, offset + 8);
        let method = u16_at(bytes, offset + 10);
        let file_type = (u32_at(bytes, offset + 38) >> 16) & 0xf000;
        if flags & !0x808 != 0
            || (method != 0 && method != 8)
            || (file_type != 0
                && file_type != 0x8000
                && !(name == "assets/" && file_type == 0x4000))
        {
            return invalid("encrypted, linked or unsupported entry");
        }
        let size = u32_at(bytes, offset + 24);
        let limit = match name {
            "theme.json" => MANIFEST_BYTES as u64,
            "assets/" => 0,
            _ => ASSET_BYTES,
        };
        expanded += size as u64;
        if size as u64 > limit || expanded > EXTRACTED_BYTES {
            return invalid("expanded size limit");
        }
        result.push(Entry {
            name: name.to_string(),
            size,
            compressed: u32_at(bytes, offset + 20),
            crc: u32_at(bytes, offset + 16),
            offset: u32_at(bytes, offset + 42),
            flags,
            method,
        });
        offset = next;
    }
    if offset != end || !names.contains("theme.json") {
        return invalid("missing theme.json or extra directory bytes");
    }
    result.sort_by_key(|e| e.offset);
    Ok((result, directory))
}

/// Raw DEFLATE, never writing past `size` bytes whatever the stream claims.
fn inflate(input: &[u8], size: usize) -> anyhow::Result<Vec<u8>> {
    let mut out = vec![0u8; size + 1];
    let mut inflater = flate2::Decompress::new(false);
    loop {
        let before_in = inflater.total_in() as usize;
        let before_out = inflater.total_out() as usize;
        let status = inflater
            .decompress(
                &input[before_in..],
                &mut out[before_out..],
                flate2::FlushDecompress::Finish,
            )
            .map_err(|_| InvalidPackage("size or checksum mismatch".into()))?;
        let written = inflater.total_out() as usize;
        if written > size {
            return invalid("actual expansion exceeds declared size");
        }
        match status {
            flate2::Status::StreamEnd => break,
            _ if inflater.total_in() as usize == before_in && written == before_out => {
                return invalid("size or checksum mismatch");
            }
            _ => {}
        }
    }
    out.truncate(inflater.total_out() as usize);
    Ok(out)
}

/// Unpack and check a whole package (`unpackTheme`).
pub fn unpack(bytes: &[u8]) -> anyhow::Result<ThemePack> {
    let (entries, directory) = entries(bytes)?;
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    let mut expected = 0usize;
    for entry in &entries {
        let offset = entry.offset as usize;
        if offset != expected || offset + 30 > directory || u32_at(bytes, offset) != LOCAL {
            return invalid("overlapping or missing local entry");
        }
        let name_len = u16_at(bytes, offset + 26) as usize;
        let start = offset + 30 + name_len + u16_at(bytes, offset + 28) as usize;
        let end = start + entry.compressed as usize;
        if end > directory {
            return invalid("truncated entry data");
        }
        let name = std::str::from_utf8(&bytes[offset + 30..offset + 30 + name_len]).ok();
        if name != Some(entry.name.as_str())
            || u16_at(bytes, offset + 6) != entry.flags
            || u16_at(bytes, offset + 8) != entry.method
        {
            return invalid("local entry disagrees with directory");
        }
        if entry.flags & 8 == 0
            && (u32_at(bytes, offset + 14) != entry.crc
                || u32_at(bytes, offset + 18) != entry.compressed
                || u32_at(bytes, offset + 22) != entry.size)
        {
            return invalid("local sizes disagree");
        }
        expected = end;
        if entry.flags & 8 != 0 {
            if expected + 12 > directory {
                return invalid("missing data descriptor");
            }
            if u32_at(bytes, expected) == DESCRIPTOR {
                expected += 4;
            }
            if expected + 12 > directory
                || u32_at(bytes, expected) != entry.crc
                || u32_at(bytes, expected + 4) != entry.compressed
                || u32_at(bytes, expected + 8) != entry.size
            {
                return invalid("invalid data descriptor");
            }
            expected += 12;
        }
        let data = &bytes[start..end];
        let output = if entry.method == 0 {
            if data.len() != entry.size as usize {
                return invalid("size or checksum mismatch");
            }
            data.to_vec()
        } else {
            inflate(data, entry.size as usize)?
        };
        if output.len() != entry.size as usize || crc32fast::hash(&output) != entry.crc {
            return invalid("size or checksum mismatch");
        }
        files.insert(entry.name.clone(), output);
    }
    if expected != directory {
        return invalid("unreferenced bytes");
    }
    let manifest_json: Arc<[u8]> = files.remove("theme.json").unwrap_or_default().into();
    let manifest = parse_manifest(&manifest_json)?;
    let mut assets = BTreeMap::new();
    let mut declared: BTreeSet<&str> = ["assets/"].into_iter().collect();
    for (id, asset) in &manifest.assets {
        let Some(path) = &asset.path else {
            return invalid("offline packages must contain their images");
        };
        let Some(content) = files.get(path) else {
            return invalid("declared image is missing");
        };
        check_image(path, asset.sha256.as_deref(), content)?;
        declared.insert(path);
        assets.insert(id.clone(), Arc::<[u8]>::from(content.as_slice()));
    }
    if files.keys().any(|name| !declared.contains(name.as_str())) {
        return invalid("undeclared files");
    }
    Ok(ThemePack {
        manifest,
        manifest_json,
        assets,
    })
}

/// A declared digest matches, the bytes are the format the extension says,
/// and the picture stays under [`IMAGE_PIXELS`].
pub fn check_image(path: &str, sha256: Option<&str>, bytes: &[u8]) -> anyhow::Result<()> {
    use sha2::Digest as _;
    if let Some(expected) = sha256 {
        let actual = hex::encode(sha2::Sha256::digest(bytes));
        if actual != expected {
            return invalid(&format!("{path} does not match its sha256"));
        }
    }
    let format = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("png") => image::ImageFormat::Png,
        Some("jpg" | "jpeg") => image::ImageFormat::Jpeg,
        Some("webp") => image::ImageFormat::WebP,
        _ => return invalid("unsupported image"),
    };
    if image::guess_format(bytes).ok() != Some(format) {
        return invalid(&format!("{path} is not the image its name says"));
    }
    let (w, h) = image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| InvalidPackage(format!("{path} cannot be read")))?;
    if w == 0 || h == 0 || w as u64 * h as u64 > IMAGE_PIXELS {
        return invalid(&format!("{path} exceeds 16 million pixels"));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stored (or deflated) ZIP the way `muqun-theme pack` writes one.
    pub(crate) fn zip(files: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
        use std::io::Write as _;
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let (method, body) = if deflate {
                let mut e =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(data).unwrap();
                (8u16, e.finish().unwrap())
            } else {
                (0u16, data.to_vec())
            };
            let crc = crc32fast::hash(data);
            let offset = out.len() as u32;
            out.extend_from_slice(&LOCAL.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 4]); // time, date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&body);

            central.extend_from_slice(&CENTRAL.to_le_bytes());
            central.extend_from_slice(&0x031eu16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 4]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 4]); // extra, comment
            central.extend_from_slice(&[0; 4]); // disk, internal attrs
            central.extend_from_slice(&(0o100644u32 << 16).to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let directory = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&EOCD.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&directory.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// A 2×2 PNG.
    pub(crate) fn png() -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    /// Cover Courier's manifest with `assets`/decoration replaced by one
    /// preview at `assets/preview.png`.
    pub(crate) fn manifest_with_preview(id: &str, sha256: Option<&str>) -> Vec<u8> {
        let mut value: serde_json::Value =
            serde_json::from_str(crate::theme::COVER_COURIER_JSON).unwrap();
        let obj = value.as_object_mut().unwrap();
        for key in ["decoration", "variantDecorations", "icons", "homeIdentity"] {
            obj.remove(key);
        }
        obj.insert("id".into(), id.into());
        obj.insert("preview".into(), "preview".into());
        let mut asset = serde_json::json!({ "path": "assets/preview.png" });
        if let Some(sha) = sha256 {
            asset["sha256"] = sha.into();
        }
        obj.insert("assets".into(), serde_json::json!({ "preview": asset }));
        serde_json::to_vec(&value).unwrap()
    }

    pub(crate) fn valid_package(id: &str) -> Vec<u8> {
        let png = png();
        let manifest = manifest_with_preview(id, None);
        zip(
            &[("theme.json", &manifest), ("assets/preview.png", &png)],
            false,
        )
    }

    fn err(bytes: &[u8]) -> String {
        unpack(bytes).unwrap_err().to_string()
    }

    #[test]
    fn a_valid_package_unpacks_with_its_assets() {
        for deflate in [false, true] {
            let png = png();
            let manifest = manifest_with_preview("demo", None);
            let pack = unpack(&zip(
                &[("theme.json", &manifest), ("assets/preview.png", &png)],
                deflate,
            ))
            .unwrap();
            assert_eq!(pack.manifest.id, "demo");
            assert_eq!(&*pack.asset("preview").unwrap(), png.as_slice());
            assert_eq!(&*pack.manifest_json, manifest.as_slice());
        }
    }

    #[test]
    fn every_real_package_in_the_themes_repository_unpacks() {
        let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes/dist");
        let Ok(dir) = std::fs::read_dir(&dist) else {
            eprintln!("{} not present; skipped", dist.display());
            return;
        };
        let mut seen = 0;
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("muqun-theme") {
                continue;
            }
            let pack = unpack(&std::fs::read(&path).unwrap())
                .unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
            assert!(!pack.assets.is_empty());
            seen += 1;
        }
        assert!(seen > 0);
    }

    #[test]
    fn path_traversal_and_foreign_names_are_refused() {
        let manifest = manifest_with_preview("demo", None);
        let png = png();
        for name in [
            "../theme.json",
            "assets/../../etc/passwd.png",
            "/abs.png",
            "assets\\x.png",
            "assets/sub/x.png",
            "assets/x.svg",
            "README.md",
            "theme.json/",
        ] {
            let bytes = zip(
                &[
                    ("theme.json", &manifest),
                    ("assets/preview.png", &png),
                    (name, b"x"),
                ],
                false,
            );
            assert!(err(&bytes).contains("unsupported or unsafe path"), "{name}");
        }
    }

    #[test]
    fn structural_lies_are_refused() {
        let good = valid_package("demo");
        assert!(err(&good[..good.len() - 1]).contains("missing ZIP directory"));
        assert!(err(b"PK").contains("truncated ZIP"));

        let mut flipped = good.clone();
        let data = 30 + "theme.json".len() + 5;
        flipped[data] ^= 0xff;
        assert!(err(&flipped).contains("checksum mismatch"));

        let png = png();
        let manifest = manifest_with_preview("demo", None);
        let dup = zip(
            &[
                ("theme.json", &manifest),
                ("assets/preview.png", &png),
                ("assets/PREVIEW.png", &png),
            ],
            false,
        );
        assert!(err(&dup).contains("duplicate path"));

        let extra = zip(
            &[
                ("theme.json", &manifest),
                ("assets/preview.png", &png),
                ("assets/other.png", &png),
            ],
            false,
        );
        assert!(err(&extra).contains("undeclared files"));

        let missing = zip(&[("theme.json", &manifest)], false);
        assert!(err(&missing).contains("declared image is missing"));

        let no_manifest = zip(&[("assets/preview.png", &png)], false);
        assert!(err(&no_manifest).contains("missing theme.json"));
    }

    #[test]
    fn a_symlink_entry_is_refused() {
        let mut bytes = valid_package("demo");
        // The first central entry's external attributes: make it a link.
        let central = bytes
            .windows(4)
            .position(|w| w == CENTRAL.to_le_bytes())
            .unwrap();
        bytes[central + 38..central + 42].copy_from_slice(&(0o120777u32 << 16).to_le_bytes());
        assert!(err(&bytes).contains("encrypted, linked or unsupported entry"));
    }

    #[test]
    fn asset_digests_and_formats_are_checked() {
        let png = png();
        let wrong = "0".repeat(64);
        let manifest = manifest_with_preview("demo", Some(&wrong));
        let bytes = zip(
            &[("theme.json", &manifest), ("assets/preview.png", &png)],
            false,
        );
        assert!(err(&bytes).contains("does not match its sha256"));

        use sha2::Digest as _;
        let right = hex::encode(sha2::Sha256::digest(&png));
        let manifest = manifest_with_preview("demo", Some(&right));
        let bytes = zip(
            &[("theme.json", &manifest), ("assets/preview.png", &png)],
            false,
        );
        assert!(unpack(&bytes).is_ok());

        let manifest = manifest_with_preview("demo", None);
        let bytes = zip(
            &[
                ("theme.json", &manifest),
                ("assets/preview.png", b"not an image"),
            ],
            false,
        );
        assert!(err(&bytes).contains("is not the image its name says"));
    }

    fn manifest_err(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value: serde_json::Value =
            serde_json::from_str(crate::theme::COVER_COURIER_JSON).unwrap();
        edit(&mut value);
        parse_manifest(&serde_json::to_vec(&value).unwrap())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn the_shipped_manifest_parses_and_unknown_top_level_keys_are_ignored() {
        let mut value: serde_json::Value =
            serde_json::from_str(crate::theme::COVER_COURIER_JSON).unwrap();
        value["somethingFromTheFuture"] = serde_json::json!({ "x": 1 });
        let manifest = parse_manifest(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(manifest.id, "cover-courier");
        assert!(manifest.desktop.is_none());
    }

    #[test]
    fn manifest_rules_match_the_app() {
        assert!(manifest_err(|v| v["id"] = "Bad Id".into()).contains("id:"));
        assert!(manifest_err(|v| v["format"] = "other".into()).contains("format:"));
        assert!(manifest_err(|v| v["version"] = "1.0".into()).contains("version:"));
        assert!(manifest_err(|v| v["name"] = "<b>".into()).contains("name:"));
        assert!(
            manifest_err(|v| v["variants"]["light"]["colors"]["primary"] = "#abc".into())
                .contains("variants.light.colors.primary")
        );
        assert!(
            manifest_err(|v| v["variants"]["dark"]["colors"]["primarySubtle"] = "red".into())
                .contains("variants.dark.colors.primarySubtle")
        );
        assert!(manifest_err(|v| {
            v["variants"]["dark"]["colors"]["extra"] = "#000000".into();
        })
        .contains("unknown field"));
        assert!(manifest_err(|v| {
            v["variants"]["dark"]["terminal"]["ansi"][3] = "#12345".into();
        })
        .contains("terminal.ansi.3"));
        assert!(manifest_err(|v| v["preview"] = "nope".into()).contains("Unknown asset: nope"));
        assert!(manifest_err(|v| {
            v["assets"]["evil"] = serde_json::json!({ "path": "../x.png" });
        })
        .contains("assets.evil.path"));
        assert!(manifest_err(|v| v["schemaVersion"] = 3.into()).contains("needs a newer version"));
        assert!(parse_manifest(b"{")
            .unwrap_err()
            .to_string()
            .contains("complete JSON"));
        assert!(parse_manifest(&vec![b' '; MANIFEST_BYTES + 1])
            .unwrap_err()
            .to_string()
            .contains("256 KiB"));
    }

    #[test]
    fn the_v2_desktop_block_is_read_leniently() {
        let mut value: serde_json::Value =
            serde_json::from_str(crate::theme::COVER_COURIER_JSON).unwrap();
        value["schemaVersion"] = 2.into();
        value["desktop"] = serde_json::json!({
            "chrome": { "titlebar": "unified", "backgroundOpacity": 0.96 },
            "effects": { "glass": true, "blur": 18, "shadows": "soft", "motion": "sparkly" },
            "typography": { "ui": "Inter", "mono": "JetBrains Mono", "scale": 9 },
            "density": "compact",
            "radius": "md",
            "cover": { "use": "sidebar" }
        });
        let m = parse_manifest(&serde_json::to_vec(&value).unwrap()).unwrap();
        let d = m.desktop.unwrap();
        assert_eq!(d.titlebar, Some(Titlebar::Unified));
        assert_eq!(d.background_opacity, Some(0.96));
        assert_eq!(d.glass, Some(true));
        assert_eq!(d.shadows, Some(Shadows::Soft));
        assert_eq!(d.motion, None, "an unknown word is dropped");
        assert_eq!(d.scale, None, "an out-of-range scale is dropped");
        assert_eq!(d.ui_font.as_deref(), Some("Inter"));
        assert_eq!(d.density, Some(Density::Compact));
        assert_eq!(d.cover, Some(CoverUse::Sidebar));
    }

    #[test]
    fn the_cover_prefers_artwork_then_wallpaper_then_preview() {
        let manifest = parse_manifest(crate::theme::COVER_COURIER_JSON.as_bytes()).unwrap();
        assert_eq!(manifest.cover_asset(false), Some("courier-light"));
        assert_eq!(manifest.cover_asset(true), Some("courier-dark"));
        let mut value: serde_json::Value =
            serde_json::from_str(crate::theme::COVER_COURIER_JSON).unwrap();
        value["homeIdentity"]["artwork"] = serde_json::json!({ "mode": "hidden" });
        let hidden = parse_manifest(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(hidden.cover_asset(true), Some("paper-dark"));
        value.as_object_mut().unwrap().remove("variantDecorations");
        let bare = parse_manifest(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(bare.cover_asset(true), Some("preview"));
    }

    #[test]
    fn a_wide_window_shows_the_regular_replacement_with_its_own_placement() {
        let mut value: serde_json::Value =
            serde_json::from_str(crate::theme::COVER_COURIER_JSON).unwrap();
        value["decoration"]["home.wallpaper"] = serde_json::json!({
            "asset": "paper-light", "fit": "cover", "opacity": 0.4,
            "focalPoint": { "x": 0.5, "y": 0.0 },
            "regular": { "asset": "paper-dark", "fit": "contain", "focalPoint": { "x": 0.2, "y": 0.8 } }
        });
        value["variantDecorations"]["light"]
            .as_object_mut()
            .unwrap()
            .remove("home.wallpaper");
        let m = parse_manifest(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(m.slot_asset("home.wallpaper", false), Some("paper-dark"));
        let look = m.slot_look("home.wallpaper", false).unwrap();
        // Its own fit and focal point; the opacity it leaves out is the slot's.
        assert!(!look.cover);
        assert_eq!(look.focal, [0.2, 0.8]);
        assert_eq!(look.opacity, 0.4);
    }

    #[test]
    fn app_only_blocks_that_do_not_read_are_dropped_not_fatal() {
        let mut value: serde_json::Value =
            serde_json::from_str(crate::theme::COVER_COURIER_JSON).unwrap();
        value["icons"] = serde_json::json!({ "chrome.settings": { "asset": 3 } });
        value["homePresentation"] = serde_json::json!("tall");
        value["iconDirections"] = serde_json::json!({ "home.arrow": "up-right" });
        let m = parse_manifest(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(m.icons.is_none() && m.home_presentation.is_none());
        assert_eq!(
            m.icon_directions
                .unwrap()
                .get("home.arrow")
                .map(String::as_str),
            Some("up-right")
        );
    }
}
