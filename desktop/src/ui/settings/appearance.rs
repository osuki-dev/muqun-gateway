//! Settings → Appearance: the current theme, then how the window draws it.

use rune::{col, el, row as hrow, text, El};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Slider};

use super::{choice, flag, group, row, SettingsTab};
use crate::config::{Appearance, ConfigEdit, Density, Scheme, Shape};
use crate::ui::copy::{tr, tr_ref};
use crate::ui::UiState;

/// The current theme as a picture: its Home art (else its wallpaper) across
/// a banner, its name and line over a scrim on the near side, and the way to
/// the gallery. A theme without art shows its colours. Lit, not outlined.
fn banner(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let look = s.look();
    let scheme = s.scheme();
    let (name, line) = crate::theme::apply::current_title()
        .unwrap_or_else(|| (s.cfg.appearance.theme_id.clone(), None));
    let line = line.unwrap_or_default();
    const H: f32 = 176.0;
    const ART_W: f32 = 460.0;
    let radius = k.radius.lg;
    let art = crate::theme::apply::home_artwork(scheme)
        .or_else(|| crate::theme::apply::backdrop(scheme, true));
    let mut banner = el()
        .key("theme-banner")
        .w_full()
        .h(H)
        .radius(radius)
        .clip_rounded(radius)
        .fill(look.emphasis(k.c.layer_02, k.c.bg_accent));
    if let (Some(images), Some(art)) = (s.images.as_ref(), art) {
        // A painting fills the far side, dissolving under the name; a
        // cut-out stands whole beside it.
        banner = banner.child(if art.look.cover {
            crate::ui::art::cover(images, &art, "theme-banner-art", ART_W, H)
                .clip_fade(ART_W * 0.35, 0.0, 0.0, 0.0)
                .pin(None, Some(0.0), Some(0.0), None)
        } else {
            let (pic, _) = crate::ui::art::contained(images, &art, H * 1.7, H - 8.0);
            pic.key("theme-banner-art")
                .pin(None, None, Some(16.0), Some(0.0))
        });
    }
    let curr_comp = s.current_computer();
    let comp_name = s.computer_name(curr_comp);
    let theme_tag = format!("{} · {}", tr(language, "Theme").to_uppercase(), comp_name);
    let copy = col()
        .key("theme-banner-copy")
        .h_full()
        .w_full()
        .justify_end()
        .gap(4.0)
        .px(20.0)
        .py(18.0)
        .linear_gradient(
            std::f32::consts::FRAC_PI_2,
            [
                (0.0, k.c.bg_base.alpha(0.92)),
                (0.45, k.c.bg_base.alpha(0.7)),
                (0.7, k.c.bg_base.alpha(0.0)),
            ],
        )
        .child(
            text(theme_tag)
                .font(rune::TextStyle {
                    tracking: 1.2,
                    ..k.ty.small_medium
                })
                .color(k.c.text_accent),
        )
        .child(
            text(name)
                .key("theme-banner-name")
                .font(rune::TextStyle {
                    size: 22.0,
                    line_height: 28.0,
                    ..k.ty.ui_medium
                })
                .color(k.c.text_base),
        )
        .children((!line.is_empty()).then(|| {
            text(line)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .max_w(380.0)
                .max_lines(2)
        }))
        .child(
            hrow()
                .pt(10.0)
                .gap(8.0)
                .children(crate::theme::omarchy::available().then(|| {
                    Button::new(tr_ref(language, "Follow Omarchy theme"))
                        .variant(ButtonVariant::Neutral)
                        .control_size(ControlSize::Small)
                        .disabled(s.cfg.appearance.theme_id == crate::theme::omarchy::ID)
                        .on_press(|s: &mut UiState| {
                            s.send(crate::model::commands::Command::ThemeApply(
                                crate::theme::omarchy::ID.into(),
                                Scheme::System,
                            ));
                        })
                        .build(k)
                        .key("follow-omarchy-theme")
                }))
                .child(
                    Button::new(tr_ref(language, "Browse themes"))
                        .variant(ButtonVariant::Neutral)
                        .control_size(ControlSize::Small)
                        .on_press(|s: &mut UiState| super::open(s, SettingsTab::Themes))
                        .build(k)
                        .key("goto-themes"),
                ),
        );
    banner.child(copy.pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)))
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let ap = &s.cfg.appearance;
    let scheme = match ap.scheme {
        Scheme::Light => 0,
        Scheme::Dark => 1,
        Scheme::System => 2,
    };
    let density = usize::from(ap.density == Density::Compact);
    let (motion, glass) = (ap.reduced_motion, ap.reduced_transparency);
    let motion_title = tr_ref(language, "Reduce motion");
    let glass_title = tr_ref(language, "Reduce transparency");
    vec![
        banner(s, k),
        group(
            k,
            "appearance-look",
            tr_ref(language, "Look"),
            vec![
                row(
                    k,
                    "scheme",
                    tr_ref(language, "Color scheme"),
                    tr_ref(language, "Follow the system, or keep it light or dark."),
                    if ap.theme_id == crate::theme::omarchy::ID {
                        text(tr_ref(language, "Controlled by Omarchy"))
                            .font(k.ty.small)
                            .color(k.c.text_muted)
                    } else {
                        choice(
                            k,
                            "scheme",
                            &[
                                tr_ref(language, "Light"),
                                tr_ref(language, "Dark"),
                                tr_ref(language, "System"),
                            ],
                            scheme,
                            |s, i| s.set_scheme([Scheme::Light, Scheme::Dark, Scheme::System][i]),
                        )
                    },
                ),
                row(
                    k,
                    "text-size",
                    tr_ref(language, "Text size"),
                    tr_ref(language, "Labels, messages and code scale together."),
                    choice(
                        k,
                        "text-size",
                        &["90%", "100%", "110%", "120%"],
                        Appearance::TEXT_SCALES
                            .iter()
                            .position(|v| (v - ap.text_scale()).abs() < 0.01)
                            .unwrap_or(1),
                        |s, i| s.edit(ConfigEdit::TextScale(Appearance::TEXT_SCALES[i])),
                    ),
                ),
                row(
                    k,
                    "density",
                    tr_ref(language, "Density"),
                    tr_ref(language, "Row heights and spacing."),
                    choice(
                        k,
                        "density",
                        &[tr_ref(language, "Comfortable"), tr_ref(language, "Compact")],
                        density,
                        |s, i| {
                            s.edit(ConfigEdit::Density(
                                [Density::Comfortable, Density::Compact][i],
                            ))
                        },
                    ),
                ),
                row(
                    k,
                    "shape",
                    tr_ref(language, "Corners"),
                    tr_ref(language, "How rounded panels, cards and controls are."),
                    choice(
                        k,
                        "shape",
                        &[
                            tr_ref(language, "Sharp"),
                            tr_ref(language, "Soft"),
                            tr_ref(language, "Round"),
                        ],
                        [Shape::Sharp, Shape::Soft, Shape::Round]
                            .iter()
                            .position(|v| *v == ap.shape)
                            .unwrap_or(1),
                        |s, i| {
                            s.edit(ConfigEdit::Shape(
                                [Shape::Sharp, Shape::Soft, Shape::Round][i],
                            ))
                        },
                    ),
                ),
                row(
                    k,
                    "window-opacity",
                    tr_ref(language, "Window opacity"),
                    tr_ref(language, "Adjust transparency across the entire desktop."),
                    {
                        let custom = ap.custom_opacity();
                        let pct = (custom.unwrap_or(Appearance::DEFAULT_OPACITY) * 100.0).round();
                        let label_text = if glass {
                            tr(language, "Solid (100%)")
                        } else if custom.is_some() {
                            format!("{pct:.0}%")
                        } else {
                            format!("{} · {pct:.0}%", tr_ref(language, "Auto"))
                        };
                        hrow()
                            .gap(10.0)
                            .center_y()
                            .children((custom.is_some() && !glass).then(|| {
                                Button::new(tr_ref(language, "Reset"))
                                    .variant(ButtonVariant::Ghost)
                                    .control_size(ControlSize::Small)
                                    .on_press(|s: &mut UiState| {
                                        s.edit(ConfigEdit::WindowOpacity(None));
                                    })
                                    .build(k)
                                    .key("window-opacity-reset")
                            }))
                            .child(
                                text(label_text)
                                    .font(k.ty.small_medium)
                                    .color(if glass {
                                        k.c.text_faint
                                    } else {
                                        k.c.text_muted
                                    })
                                    .w(72.0),
                            )
                            .child(
                                Slider::new("window-opacity", if glass { 100.0 } else { pct })
                                    .range((Appearance::MIN_OPACITY * 100.0)..=100.0)
                                    .step(1.0)
                                    .label(tr_ref(language, "Window opacity"))
                                    .hide_label()
                                    .value_text(format!("{pct:.0}%"))
                                    .width(156.0)
                                    .disabled(glass)
                                    .on_change(|s: &mut UiState, value, _done| {
                                        let alpha = (value.round() / 100.0)
                                            .clamp(Appearance::MIN_OPACITY, 1.0);
                                        s.edit(ConfigEdit::WindowOpacity(Some(alpha)));
                                    })
                                    .build(k),
                            )
                    },
                ),
            ],
        ),
        group(
            k,
            "appearance-access",
            tr_ref(language, "Accessibility"),
            vec![
                row(
                    k,
                    "motion",
                    motion_title,
                    tr_ref(language, "Large movements become short fades."),
                    flag(k, "motion", motion, motion_title, move |s| {
                        s.edit(ConfigEdit::ReducedMotion(!motion))
                    }),
                ),
                row(
                    k,
                    "glass",
                    glass_title,
                    tr_ref(language, "Glass surfaces and cover art become solid."),
                    flag(k, "glass", glass, glass_title, move |s| {
                        s.edit(ConfigEdit::ReducedTransparency(!glass))
                    }),
                ),
            ],
        ),
    ]
}
