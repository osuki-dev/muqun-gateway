//! Settings → Appearance: the current theme, then how the window draws it.

use rune::{col, el, row as hrow, text, El};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

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
    let radius = crate::theme::look::CARD_RADIUS;
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
            text(tr(language, "Current theme", "当前主题").to_uppercase())
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
            hrow().pt(10.0).child(
                Button::new(tr_ref(language, "Browse themes", "浏览主题"))
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
    let motion_title = tr_ref(language, "Reduce motion", "减少动态");
    let glass_title = tr_ref(language, "Reduce transparency", "减少透明");
    vec![
        banner(s, k),
        group(
            k,
            "appearance-look",
            tr_ref(language, "Look", "外观"),
            vec![
                row(
                    k,
                    "scheme",
                    tr_ref(language, "Color scheme", "配色"),
                    tr_ref(
                        language,
                        "Follow the system, or keep it light or dark.",
                        "跟随系统，或固定浅色 / 深色。",
                    ),
                    choice(
                        k,
                        "scheme",
                        &[
                            tr_ref(language, "Light", "浅色"),
                            tr_ref(language, "Dark", "深色"),
                            tr_ref(language, "System", "系统"),
                        ],
                        scheme,
                        |s, i| s.set_scheme([Scheme::Light, Scheme::Dark, Scheme::System][i]),
                    ),
                ),
                row(
                    k,
                    "text-size",
                    tr_ref(language, "Text size", "文字大小"),
                    tr_ref(
                        language,
                        "Labels, messages and code scale together.",
                        "文字、消息与代码一起缩放。",
                    ),
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
                    tr_ref(language, "Density", "密度"),
                    tr_ref(language, "Row heights and spacing.", "行高与间距。"),
                    choice(
                        k,
                        "density",
                        &[
                            tr_ref(language, "Comfortable", "舒适"),
                            tr_ref(language, "Compact", "紧凑"),
                        ],
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
                    tr_ref(language, "Corners", "圆角"),
                    tr_ref(
                        language,
                        "How rounded panels, cards and controls are.",
                        "面板、卡片与控件的圆角程度。",
                    ),
                    choice(
                        k,
                        "shape",
                        &[
                            tr_ref(language, "Sharp", "锐利"),
                            tr_ref(language, "Soft", "柔和"),
                            tr_ref(language, "Round", "圆润"),
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
            ],
        ),
        group(
            k,
            "appearance-access",
            tr_ref(language, "Accessibility", "辅助功能"),
            vec![
                row(
                    k,
                    "motion",
                    motion_title,
                    tr_ref(
                        language,
                        "Large movements become short fades.",
                        "大幅动画改为短暂淡入淡出。",
                    ),
                    flag(k, "motion", motion, motion_title, move |s| {
                        s.edit(ConfigEdit::ReducedMotion(!motion))
                    }),
                ),
                row(
                    k,
                    "glass",
                    glass_title,
                    tr_ref(
                        language,
                        "Glass surfaces and cover art become solid.",
                        "玻璃表面与封面插图改为不透明。",
                    ),
                    flag(k, "glass", glass, glass_title, move |s| {
                        s.edit(ConfigEdit::ReducedTransparency(!glass))
                    }),
                ),
            ],
        ),
    ]
}
