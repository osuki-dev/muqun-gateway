//! GPU effect layers through Rune's compositor hook: every `El::effect(mark)`
//! slot in the tree becomes one rune-fx layer drawn at its rect.
//!
//! Two kinds, chosen by the slot's mark; the slot's `effect_data` carries the
//! colour (linear RGB) and one parameter, so the compositor needs no app
//! state. Views emit slots only through [`UiState::fx_on`]: nothing under
//! reduce motion or reduce transparency. [`Fx::animating`] is true only while
//! a drawn layer moves on its own (the splash's liquid metal, a working
//! composer's breathing rim), so an idle window stays idle.

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
/// A breathing neon rim on the slot's edge in `data[0..3]`, intensity `data[3]`.
pub const RIM: u32 = 2;
/// Liquid metal filling the slot, tinted toward `data[0..3]`; `data[3]` is
/// the flow (0 holds a polished, still surface that costs no frames).
pub const METAL: u32 = 3;

/// The reach a glow slot needs past its rect.
pub const GLOW_BLEED: f32 = 48.0;

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

/// A glow slot pinned over its parent's rect (the parent must be positioned).
pub fn glow_slot(key: &str, color: rune::Color, intensity: f32, radius: f32) -> El {
    rune::el()
        .key(key.to_string())
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .radius(radius)
        .effect(GLOW)
        .effect_bleed(GLOW_BLEED)
        .effect_data(data(color, intensity))
        .a11y_hidden()
}

/// A neon rim slot pinned over its parent's rect.
pub fn rim_slot(key: &str, color: rune::Color, intensity: f32, radius: f32) -> El {
    rune::el()
        .key(key.to_string())
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .radius(radius)
        .effect(RIM)
        .effect_bleed(24.0)
        .effect_data(data(color, intensity))
        .a11y_hidden()
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

fn layer(kind: u32, d: [f32; 4], shape: Shape, t: f32, reduced: bool) -> Option<FxLayer> {
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
        RIM => rune_fx::Fx::glow(GlowParams {
            style: GlowStyle::Neon,
            color: to_rgb(d),
            core: Rgb([
                (d[0] + 0.6).min(1.0),
                (d[1] + 0.6).min(1.0),
                (d[2] + 0.6).min(1.0),
            ]),
            intensity: d[3].clamp(0.0, 2.0),
            radius: 10.0,
            rim_width: 0.8,
            breathe: 0.45,
            breathe_period: 2.4,
            ..Default::default()
        }),
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
        for (n, i) in f.rt.instances.iter().enumerate() {
            if i.params[1] != KIND_EFFECT {
                continue;
            }
            let kind = f.rt.marks.get(n).copied().unwrap_or(0);
            let k = i.params[3].max(1e-3);
            let [x, y, w, h] = i.rect.map(|v| v / k);
            let r = i.radii.iter().fold(0.0f32, |a, v| a.max(v.abs())) / k;
            let Some(l) = layer(kind, i.uv, Shape::rounded(x, y, w, h, r), t, reduced) else {
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
