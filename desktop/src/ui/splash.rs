//! Native counterpart of the App's launch-intro-timeline: ignite, bloom,
//! hero lift, typed theme name, then a dissolve into the loaded workspace.
//! Uses the applied theme only. The frame clock also makes preview recordings
//! exercise the same timeline as a real window.

use super::art;
use crate::theme::{color_scheme::ColorScheme, look::Look};
use rune::{col, el, row, text, El, ImageCache, Spring};
use rune_kit::Kit;

// App motion tokens: micro 150, short 200, medium 300, long 400 ms.
const HOLD: f32 = 1.0;
const DEADLINE: f32 = 1.3;
const EXIT: f32 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplashKind {
    Boot,
    Theme,
}

#[derive(Debug, Clone)]
pub struct Splash {
    pub kind: SplashKind,
    pub elapsed: f32,
    origin: Option<f64>,
    exit_at: Option<f32>,
    landing: std::cell::Cell<Option<[f32; 4]>>,
}

impl Splash {
    pub fn start(kind: SplashKind) -> Self {
        Self {
            kind,
            elapsed: 0.0,
            origin: None,
            exit_at: None,
            landing: Default::default(),
        }
    }
    pub fn data_ready(kind: SplashKind, agents_seen: bool, hub_ready: bool) -> bool {
        kind == SplashKind::Theme || (agents_seen && hub_ready)
    }
    pub fn tick(&mut self, t: f64, agents_seen: bool, hub_ready: bool) -> bool {
        let origin = *self.origin.get_or_insert(t - self.elapsed as f64);
        self.elapsed = self.elapsed.max((t - origin).max(0.0) as f32);
        if self.exit_at.is_none()
            && ((Self::data_ready(self.kind, agents_seen, hub_ready) && self.elapsed >= HOLD)
                || self.elapsed >= DEADLINE)
        {
            self.exit_at = Some(self.elapsed);
        }
        self.exit_at.is_none_or(|at| self.elapsed - at < EXIT)
    }

    pub fn view(
        &self,
        images: &ImageCache,
        k: &Kit,
        look: &Look,
        scheme: ColorScheme,
    ) -> Option<El> {
        // Freeze the measured destination before travel starts. A late Home
        // layout must not redirect the artwork in the middle of its flight.
        if self.elapsed <= 0.2 {
            self.landing
                .set(rune::memo::window_layout_with("home-figure", |mut r| {
                    // Home lifts its stage under the empty toolbar using a
                    // visual offset; Rune's layout reads exclude transforms.
                    r[1] -= rune_kit::shell::TOOLBAR_HEIGHT;
                    r
                }));
        }
        let exit = self
            .exit_at
            .map_or(0.0, |at| ((self.elapsed - at) / EXIT).clamp(0.0, 1.0));
        if exit >= 1.0 {
            return None;
        }
        Some(stage(
            images,
            k,
            look,
            scheme,
            self.elapsed,
            self.landing.get(),
            exit,
        ))
    }
}

fn beat(t: f32, start: f32, duration: f32) -> f32 {
    ((t - start) / duration).clamp(0.0, 1.0)
}
fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

#[allow(clippy::too_many_arguments)]
fn stage(
    images: &ImageCache,
    k: &Kit,
    look: &Look,
    scheme: ColorScheme,
    t: f32,
    landing: Option<[f32; 4]>,
    exit: f32,
) -> El {
    let [w, h] =
        rune::memo::layout_with("theme-splash", |r| [r[2], r[3]]).unwrap_or([1120.0, 760.0]);
    let center = [w * 0.5, h * 0.43];
    let bloom = if look.motion {
        ease(beat(t, 0.15, 0.6))
    } else {
        1.0
    };
    let lift = if look.motion {
        ease(beat(t, 0.2, 0.6))
    } else {
        1.0
    };
    let leave = ease(exit);
    let side = (h * 0.38).clamp(120.0, 300.0).min(w * 0.65);
    // Home is already laid out underneath the opening. Land in its real
    // artwork rect, rather than fading a small logo into a different picture.
    let start = [center[0] - side * 0.5, center[1] - side * 0.5, side, side];
    let destination = landing.unwrap_or(start);
    let hero_rect =
        std::array::from_fn::<_, 4, _>(|i| start[i] + (destination[i] - start[i]) * lift);
    let hero_y = hero_rect[1];
    let radius = w.hypot(h) * 0.65 * bloom;
    let accent = k.c.bg_accent;

    let mut world = el()
        .key("launch-world")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(k.c.bg_base)
        .child(look.light("launch-primary-field", [0.3, 0.35], [0.8, 0.8], accent))
        .child(look.light("launch-info-field", [0.85, 0.75], [0.7, 0.8], k.c.info.icon));
    if let Some(wallpaper) = crate::theme::apply::backdrop(scheme, true) {
        let from = [0.0, 0.0, w, h];
        let to = rune::memo::window_layout_with("window-backdrop", |r| r).unwrap_or(from);
        let rect: [f32; 4] = std::array::from_fn(|i| from[i] + (to[i] - from[i]) * lift);
        // Keep the theme across the whole window until the shell is revealed.
        // Moving the only backdrop into the main card exposes a dark sidebar
        // sized hole before any of its controls have appeared.
        world = world.child(
            art::cover(images, &wallpaper, "launch-window-wallpaper", w, h)
                .absolute(0.0, 0.0)
                .opacity(wallpaper.look.opacity)
                .scale(1.06 - 0.06 * ease(beat(t, 0.15, 0.8))),
        );
        world = world.child(
            art::place(images, &wallpaper, "launch-wallpaper", rect[2], rect[3])
                .clip_fade(96.0, 16.0, 16.0, 16.0)
                .absolute(rect[0], rect[1])
                .opacity(wallpaper.look.opacity)
                .scale(1.06 - 0.06 * ease(beat(t, 0.15, 0.8))),
        );
    }
    // Reveal the actual pack painting through an expanding iris. The App's
    // Skia ink front is represented by a soft native mask on Desktop.
    let cover = el()
        .key("launch-iris")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(k.c.bg_base)
        .cutout_circle(center, (radius - 18.0).max(0.0), radius.max(1.0));
    let front = el()
        .key("launch-front")
        .absolute(center[0] - radius, center[1] - radius)
        .size(radius * 2.0, radius * 2.0)
        .radius(radius)
        .border(2.0, accent.alpha((1.0 - bloom) * bloom * 3.0));

    let launch_art = crate::theme::apply::launch(scheme);
    let home_art = landing.and_then(|_| crate::theme::apply::home_artwork(scheme));
    let draw_hero = |art: Option<&crate::theme::apply::Art>, key: &str, rect: [f32; 4]| {
        let [x, y, width, height] = rect;
        let picture = if let Some(art) = art {
            if landing.is_some() && !art.look.cover {
                let (picture, _) = art::contained(images, art, width, height);
                let overlap = (height * 0.1).clamp(24.0, 64.0);
                picture
                    .clip_fade(width * 0.04, 0.0, width * 0.04, overlap + height * 0.14)
                    .opacity(art.look.opacity)
            } else {
                art::place(images, art, key, width, height)
                    .clip_fade(
                        0.0,
                        0.0,
                        0.0,
                        if landing.is_some() {
                            height * 0.45
                        } else {
                            0.0
                        },
                    )
                    .opacity(art.look.opacity)
            }
        } else {
            let bytes = crate::theme::apply::identity()
                .logo
                .unwrap_or_else(|| std::sync::Arc::<[u8]>::from(crate::branding::APP_MARK_PNG));
            art::picture(images, bytes, rune::SurfaceFit::Fit).size(width, height)
        };
        el().key(key.to_string())
            .absolute(x, y)
            .size(width, height)
            .child(picture)
    };
    let mut hero = el()
        .key("launch-hero")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0));
    if let Some(home) = home_art.as_ref() {
        hero = hero
            .child(draw_hero(launch_art.as_ref(), "launch-source", hero_rect).opacity(1.0 - lift))
            .child(draw_hero(Some(home), "launch-landing", hero_rect).opacity(lift));
    } else {
        hero = hero.child(draw_hero(launch_art.as_ref(), "launch-source", hero_rect));
    }
    // The App trails copies of the actual artwork, never a frame around it.
    if look.motion && lift > 0.0 && lift < 1.0 {
        for (i, lag) in [0.06, 0.12].into_iter().enumerate() {
            let behind = ease(beat(t - lag, 0.2, 0.6));
            let rect = std::array::from_fn(|n| start[n] + (destination[n] - start[n]) * behind);
            hero = hero.child(
                draw_hero(
                    home_art.as_ref().or(launch_art.as_ref()),
                    &format!("launch-trail-{i}"),
                    rect,
                )
                .opacity((1.0 - lift) * 0.12 / (i + 1) as f32),
            );
        }
    }

    let name = crate::theme::apply::current_title()
        .map(|x| x.0)
        .unwrap_or_else(|| crate::branding::APP_NAME.to_string());
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = name.chars().count() > 32;
    let mut name: String = name.chars().take(32).collect();
    if truncated {
        name = name.chars().take(31).chain(['…']).collect();
    }
    let progress = if look.motion { beat(t, 0.3, 0.55) } else { 1.0 };
    let count = (progress * name.chars().count() as f32).floor() as usize;
    let typed: String = name.chars().take(count).collect();
    let blink = beat(t, 0.85, 0.15);
    let cursor = !(0.3..0.72).contains(&blink);
    let prompt = col()
        .key("launch-prompt")
        .absolute(0.0, h * 0.78)
        .w_full()
        .items_center()
        .gap(12.0)
        .opacity(1.0 - leave)
        .child(
            row()
                .gap(10.0)
                .center_y()
                .child(text("›").font(k.ty.mono).color(accent))
                .child(text(typed).font(k.ty.mono).color(k.c.text_base))
                .child(
                    el().size(8.0, 18.0)
                        .fill(accent)
                        .opacity(if cursor { 1.0 } else { 0.0 }),
                ),
        );

    let mut stage = el()
        .key("theme-splash")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .clip_soft(0.0)
        .motion(Spring::NONE)
        .opacity(1.0 - leave)
        .child(world)
        .child(cover)
        .child(front)
        .child(hero)
        .child(prompt);
    // A sparse trail from the hero's outline, not particles over readable text.
    if look.motion && t > 0.3 && t < HOLD {
        for i in 0..20 {
            let angle = i as f32 * std::f32::consts::TAU / 20.0;
            let travel = beat(t, 0.3 + (i % 4) as f32 * 0.035, 0.85);
            let distance = hero_rect[2].min(hero_rect[3]) * 0.45 + travel * 25.0;
            stage = stage.child(
                el().key(format!("launch-grain-{i}"))
                    .absolute(
                        hero_rect[0] + hero_rect[2] * 0.5 + angle.cos() * distance + travel * 25.0,
                        hero_y + hero_rect[3] * 0.5 + angle.sin() * distance - travel * 30.0,
                    )
                    .size(2.0 + (i % 3) as f32, 2.0)
                    .radius(1.0)
                    .fill(accent)
                    .opacity((1.0 - travel) * travel * 2.5),
            );
        }
    }
    stage
        .on_press(|s: &mut super::UiState| {
            if s.splash.as_ref().is_some_and(|intro| intro.elapsed >= 0.4) {
                s.splash = None;
            }
        })
        .a11y_hidden()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_holds_the_intro_but_a_dead_hub_cannot_block_the_workspace() {
        let mut ready = Splash::start(SplashKind::Boot);
        assert!(ready.tick(10.0, true, true));
        assert!(ready.tick(10.8, true, true));
        assert!(ready.exit_at.is_none());
        assert!(ready.tick(11.0, true, true));
        assert!(!ready.tick(11.31, true, true));
        let mut offline = Splash::start(SplashKind::Boot);
        assert!(offline.tick(0.0, false, false));
        assert!(offline.tick(1.3, false, false));
        assert!(!offline.tick(1.61, false, false));
    }
}
