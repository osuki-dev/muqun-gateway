//! GPU effect layers through Rune's compositor hook: every `El::effect(mark)`
//! slot in the tree becomes one rune-fx layer drawn at its rect.
//!
//! The slot's mark selects the effect; the slot's `effect_data` carries the
//! colour (linear RGB) and one parameter, so the compositor needs no app
//! state. Views emit slots only through [`UiState::fx_on`]: nothing under
//! reduce motion or reduce transparency. [`Fx::animating`] is true only while
//! a drawn layer moves on its own, so an idle window stays idle.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use rune::compose::{Compositor, Frame};
use rune::render::{EffectPhase, EffectSite, KIND_EFFECT};
use rune::El;
use rune_fx::{
    FillAnchor, FxFrame, FxLayer, FxOptions, FxRenderer, FxSlot, GlowParams, GlowStyle,
    LiquidMetalParams, Rgb, Shape,
};

use super::UiState;

/// A soft bloom under the slot in `data[0..3]`, intensity `data[3]`.
pub const GLOW: u32 = 1;
/// Liquid metal filling the slot, tinted toward `data[0..3]`; `data[3]` is
/// the flow (0 holds a polished, still surface that costs no frames).
pub const METAL: u32 = 3;
/// A highlight circulating around a busy composer's rounded outline.
pub const COMPOSER_RIM: u32 = 4;
/// A soft, borderless animated theme-gradient aura hugging a focused composer.
pub const FOCUS_AURA: u32 = 5;
/// A calmer borderless animated theme-gradient aura hugging a focused split pane.
pub const TILE_AURA: u32 = 6;

impl UiState {
    /// Whether views may emit effect slots this frame.
    pub fn fx_on(&self) -> bool {
        !self.cfg.appearance.reduced_motion
            && !self.cfg.appearance.reduced_transparency
            && fx_enabled()
    }
}

/// GPU effects stay on unless `MUQUN_FX=0` (a diagnostic switch) or the
/// GPU refused an allocation ([`gpu_lost`]).
pub fn fx_enabled() -> bool {
    static ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENV.get_or_init(|| std::env::var("MUQUN_FX").map_or(true, |v| v != "0")) && !gpu_lost()
}

/// Set once the GPU reported an error (out of memory, most likely): effects
/// and glass go dark and stay dark for the process, so the window keeps
/// drawing with less instead of panicking.
static GPU_LOST: AtomicBool = AtomicBool::new(false);
static GPU_ERRORS: AtomicU32 = AtomicU32::new(0);

pub fn gpu_lost() -> bool {
    GPU_LOST.load(Ordering::Relaxed)
}

/// The window's target size in device pixels, as the last frame saw it
/// (0 before the first frame). The materials size their budget by it.
static VIEWPORT: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

pub fn viewport_pixels() -> f32 {
    VIEWPORT[0].load(Ordering::Relaxed) as f32 * VIEWPORT[1].load(Ordering::Relaxed) as f32
}

/// wgpu's default uncaptured-error handler panics ("Handling wgpu errors as
/// fatal by default"). Installed once per device: an error is logged (the
/// first few in full, then one line per hundred) and downgrades the look.
fn install_error_handler(device: &wgpu::Device) {
    device.on_uncaptured_error(Arc::new(|error: wgpu::Error| {
        let n = GPU_ERRORS.fetch_add(1, Ordering::Relaxed);
        let first = !GPU_LOST.swap(true, Ordering::Relaxed);
        let kind = match &error {
            wgpu::Error::OutOfMemory { .. } => "out of memory",
            wgpu::Error::Validation { .. } => "validation",
            wgpu::Error::Internal { .. } => "internal",
        };
        if first {
            tracing::error!(
                %error,
                "GPU {kind} error; effects and glass are off for this run"
            );
        } else if n < 5 || n.is_multiple_of(100) {
            tracing::warn!(n, "GPU {kind} error (continuing): {error}");
        }
    }));
}

/// Pack a kit colour and a parameter into a slot's payload.
pub fn data(color: rune::Color, param: f32) -> [f32; 4] {
    [color.r, color.g, color.b, param]
}

/// A liquid-metal slab of `size`, tinted toward `color`, flowing at `flow`.
pub fn metal(key: &str, size: f32, radius: f32, color: rune::Color, flow: f32) -> El {
    rune::el()
        .key(key.to_string())
        .size(size, size)
        .radius(radius)
        .effect(METAL)
        .effect_data(data(color, flow))
        .a11y_hidden()
}

fn to_rgb(d: [f32; 4]) -> Rgb {
    Rgb([
        d[0].clamp(0.0, 1.0),
        d[1].clamp(0.0, 1.0),
        d[2].clamp(0.0, 1.0),
    ])
}

fn color_to_rgb(c: rune::Color) -> Rgb {
    Rgb([
        c.r.clamp(0.0, 1.0),
        c.g.clamp(0.0, 1.0),
        c.b.clamp(0.0, 1.0),
    ])
}

fn aura_triad(d: [f32; 4], dark: bool) -> [Rgb; 3] {
    let base = rune::Color {
        r: d[0].clamp(0.0, 1.0),
        g: d[1].clamp(0.0, 1.0),
        b: d[2].clamp(0.0, 1.0),
        a: 1.0,
    };
    let [c0, c1, c2] = crate::theme::look::companion_highlight_colors(base, dark);
    [color_to_rgb(c0), color_to_rgb(c1), color_to_rgb(c2)]
}

fn layer(
    kind: u32,
    d: [f32; 4],
    shape: Shape,
    t: f32,
    reduced: bool,
    dark: bool,
) -> Option<FxLayer> {
    let fx = match kind {
        GLOW => rune_fx::Fx::glow(GlowParams {
            style: GlowStyle::Soft,
            color: to_rgb(d),
            core: to_rgb(d),
            intensity: d[3].clamp(0.0, 2.0),
            radius: 26.0,
            reach: 3.0,
            occlusion: 0.1,
            ..Default::default()
        }),
        COMPOSER_RIM => {
            let [c0, c1, c2] = aura_triad(d, dark);
            // Keep busy composer aura static (`breathe: 0.0`, `aura_speed: 0.0`)
            // so `Fx::animating()` stays false and does not spin 60/120 Hz
            // full-window Metal/Vulkan redraws while an agent turn is working.
            rune_fx::Fx::glow(GlowParams {
                style: GlowStyle::Aura,
                color: c0,
                core: c1,
                third: c2,
                radius: 10.0,
                rim_width: 0.6,
                intensity: d[3].clamp(0.0, 1.2) * if dark { 0.62 } else { 0.48 },
                inner: if dark { 0.30 } else { 0.24 },
                occlusion: if dark { 0.62 } else { 0.92 },
                breathe: 0.0,
                aura_speed: 0.0,
                hot: 0.0,
                reach: 2.2,
                ..Default::default()
            })
        }
        FOCUS_AURA => {
            let [c0, c1, c2] = aura_triad(d, dark);
            // Keep idle focus aura static (`breathe: 0.0`, `aura_speed: 0.0`) so
            // `Fx::animating()` stays false at rest and does not spin 60/120 Hz
            // full-window Metal/Vulkan redraws on 4K Retina displays.
            rune_fx::Fx::glow(GlowParams {
                style: GlowStyle::Aura,
                color: c0,
                core: c1,
                third: c2,
                radius: 11.0,
                rim_width: 0.55,
                intensity: d[3].clamp(0.0, 1.2) * if dark { 0.54 } else { 0.42 },
                inner: if dark { 0.26 } else { 0.20 },
                occlusion: if dark { 0.65 } else { 0.94 },
                breathe: 0.0,
                aura_speed: 0.0,
                hot: 0.0,
                reach: 2.1,
                ..Default::default()
            })
        }
        TILE_AURA => {
            let [c0, c1, c2] = aura_triad(d, dark);
            rune_fx::Fx::glow(GlowParams {
                style: GlowStyle::Aura,
                color: c0,
                core: c1,
                third: c2,
                radius: 5.5,
                rim_width: 0.45,
                intensity: d[3].clamp(0.0, 1.0) * if dark { 0.24 } else { 0.18 },
                inner: if dark { 0.12 } else { 0.10 },
                occlusion: if dark { 0.72 } else { 0.95 },
                breathe: 0.0,
                aura_speed: 0.0,
                hot: 0.0,
                reach: 1.6,
                ..Default::default()
            })
        }
        METAL => {
            let tint = to_rgb(d);
            // Chrome lit through the theme's accent: a cool studio, a warm tint.
            let light = Rgb([
                0.55 + 0.45 * tint.0[0],
                0.55 + 0.45 * tint.0[1],
                0.55 + 0.45 * tint.0[2],
            ]);
            let flow = d[3].max(0.0);
            rune_fx::Fx::fill(
                LiquidMetalParams {
                    tint: Rgb([
                        0.45 + 0.55 * tint.0[0],
                        0.45 + 0.55 * tint.0[1],
                        0.45 + 0.55 * tint.0[2],
                    ]),
                    light,
                    flow,
                    speed: if flow > 0.0 { 0.3 } else { 0.0 },
                    scale: 60.0,
                    bevel: 8.0,
                    dome: 0.6,
                    iridescence: 0.2,
                    contrast: 1.1,
                    ..LiquidMetalParams::chrome()
                }
                .anchor(FillAnchor::Element),
            )
        }
        _ => return None,
    };
    Some(fx.on(shape).at(t).reduced(reduced))
}

/// The compositor: resources per GPU device, rebuilt when the device changes.
#[derive(Default)]
pub struct Fx {
    renderer: Option<(rune::compose::DeviceKey, FxRenderer)>,
    /// The device whose errors are routed through [`install_error_handler`].
    guarded: Option<rune::compose::DeviceKey>,
    animating: bool,
    frames: u64,
}

impl Compositor<UiState> for Fx {
    fn animating(&self) -> bool {
        self.animating
    }

    fn render(&mut self, f: &mut Frame<'_, UiState>) {
        let key = f.device_key();
        if self.guarded != Some(key) {
            install_error_handler(f.device);
            self.guarded = Some(key);
        }
        VIEWPORT[0].store(f.size.0, Ordering::Relaxed);
        VIEWPORT[1].store(f.size.1, Ordering::Relaxed);
        let t = f.rt.t as f32;
        let reduced = f.rt.reduced_motion;
        // GPU memory telemetry for `RUST_LOG=muqun_desktop=debug`: Rune's
        // isolated-layer targets and image atlas pages, every ~5 s.
        self.frames += 1;
        if self.frames % 300 == 1 && tracing::enabled!(tracing::Level::DEBUG) {
            let stats = f.ui.layer_stats();
            let pages: Vec<(u32, u32)> =
                f.rt.images
                    .pages
                    .iter()
                    .map(|p| (p.width, p.height))
                    .collect();
            let look = f.rt.state.look();
            tracing::debug!(
                viewport = ?f.size,
                blur = look.blur,
                glass = look.glass,
                layer_targets = stats.targets,
                layer_mib = stats.bytes / (1024 * 1024),
                layer_allocations = stats.allocations,
                ?pages,
                "gpu memory"
            );
        }
        let mut at: Vec<u32> = Vec::new();
        let mut layers: Vec<FxLayer> = Vec::new();
        let mut slots: Vec<FxSlot> = Vec::new();
        let rim_active = f.rt.state.demo || crate::platform::notify::window_focused();
        let dark = f.rt.state.look().dark;
        for (n, i) in f.rt.instances.iter().enumerate() {
            if i.params[1] != KIND_EFFECT {
                continue;
            }
            let kind = f.rt.marks.get(n).copied().unwrap_or(0);
            if kind == COMPOSER_RIM && (reduced || !rim_active) {
                continue;
            }
            let layer_reduced = reduced || (matches!(kind, FOCUS_AURA | TILE_AURA) && !rim_active);
            let k = i.params[3].max(1e-3);
            let [x, y, w, h] = i.rect.map(|v| v / k);
            let r = i.radii.iter().fold(0.0f32, |a, v| a.max(v.abs())) / k;
            let Some(l) = layer(
                kind,
                i.uv,
                Shape::rounded(x, y, w, h, r),
                t,
                layer_reduced,
                dark,
            ) else {
                continue;
            };
            at.push(n as u32);
            layers.push(l);
            slots.push(FxSlot::default().scale(k).clip(i.clip, i.fade));
        }
        self.animating = layers.iter().any(FxLayer::is_animating);
        if layers.is_empty() {
            f.draw_ui();
            return;
        }
        let (device, queue, size, format) = (f.device, f.queue, f.size, f.format);
        if self.renderer.as_ref().is_none_or(|(k, _)| *k != key) {
            let mut fx = FxRenderer::new(device, format, FxOptions { dither: true });
            fx.add_format(device, wgpu::TextureFormat::Rgba16Float);
            self.renderer = Some((key, fx));
        }
        let fx = &mut self.renderer.as_mut().expect("fx renderer").1;
        fx.set_srgb_blend(f.srgb_blend());
        fx.prepare_with_slots(
            device,
            queue,
            &FxFrame {
                viewport: [size.0, size.1],
                scale: 1.0,
                layers: &layers,
                backdrop: None,
                sources: &[],
            },
            &slots,
        );
        let clear = f.rt.clear_color();
        f.ui.prepare(device, queue, &mut f.rt.text.atlas, &f.rt.instances, size);
        f.ui.draw_with_backdrop(
            f.encoder,
            f.target,
            clear,
            &mut |phase, s: &EffectSite<'_>| {
                let Some(k) = at.iter().position(|n| *n == s.index) else {
                    return;
                };
                if let EffectPhase::Draw(pass) = phase {
                    fx.draw_layer_in(pass, s.format, s.origin, k);
                }
            },
        );
    }
}
