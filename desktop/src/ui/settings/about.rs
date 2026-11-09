//! Settings → About: what this is, how it is connected, where to go next.

use std::path::PathBuf;
use std::sync::Arc;

use rune::{col, el, row as hrow, text, El, SurfaceFit};
use rune_kit::{Badge, BadgeVariant, Kit};

use super::{group, link_button, row, row_below};
use crate::branding::{APP_MARK_PNG, APP_NAME};
use crate::config::Language;
use crate::ui::copy::{tr, tr_ref};
use crate::ui::UiState;

fn hub_label(language: Language, raw: &str) -> String {
    match raw {
        "" => tr(language, "Connecting…", "连接中…"),
        "local gateway" => tr(language, "Local gateway", "本机 Gateway"),
        "standalone" => tr(language, "Standalone", "独立运行"),
        other => other.to_string(),
    }
}

/// The app's mark, name, version and build, lit from behind.
fn hero(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let look = s.look();
    const MARK: f32 = 56.0;
    let mark = match s.images.as_ref() {
        Some(images) => {
            crate::ui::art::picture(images, Arc::<[u8]>::from(APP_MARK_PNG), SurfaceFit::Fill)
                .key("about-mark")
                .size(MARK, MARK)
                .radius(14.0)
                .clip_rounded(14.0)
        }
        None => el()
            .key("about-mark")
            .size(MARK, MARK)
            .radius(14.0)
            .fill(k.c.bg_accent),
    };
    let build = option_env!("MUQUN_BUILD").unwrap_or(if cfg!(debug_assertions) {
        "debug build"
    } else {
        "release build"
    });
    hrow()
        .key("about-hero")
        .w_full()
        .gap(18.0)
        .center_y()
        .child(look.glow(mark, k.c.bg_accent, 0.5).a11y_hidden())
        .child(
            col()
                .key("about-words")
                .grow(1.0)
                .shrink()
                .gap(3.0)
                .child(
                    text(APP_NAME)
                        .font(rune::TextStyle {
                            size: 20.0,
                            line_height: 26.0,
                            ..k.ty.ui_medium
                        })
                        .color(k.c.text_base),
                )
                .child(
                    text(tr(
                        language,
                        "Coding agents on this computer, at your desk.",
                        "在桌面上使用这台电脑上的编程 Agents。",
                    ))
                    .font(k.ty.ui)
                    .color(k.c.text_muted)
                    .wrap(),
                )
                .child(
                    text(format!(
                        "{} {} · {build} · rune {}",
                        tr_ref(language, "Version", "版本"),
                        env!("CARGO_PKG_VERSION"),
                        crate::branding::RUNE_REV
                    ))
                    .key("about-version")
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .wrap(),
                ),
        )
}

fn license_row(k: &Kit, name: &str, license: &str, detail: &str) -> El {
    row(
        k,
        &format!("lic-{}", name.replace([' ', ','], "-")),
        name,
        detail,
        Badge::new(license).variant(BadgeVariant::Subtle).build(k),
    )
}

fn path_row(k: &Kit, key: &str, title: &str, path: String) -> El {
    row_below(
        k,
        &format!("path-{key}"),
        title,
        "",
        text(path)
            .font(k.ty.mono)
            .color(k.c.text_muted)
            .wrap()
            .w_full(),
    )
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let (config, state) =
        crate::platform::directories().unwrap_or_else(|_| (PathBuf::new(), PathBuf::new()));
    let live = !s.hub_label.is_empty();
    let hub = hrow()
        .key("hub")
        .gap(8.0)
        .center_y()
        .child(s.look().spark(
            el().size(7.0, 7.0).radius(3.5).fill(if live {
                k.c.success.icon
            } else {
                k.c.text_faint
            }),
            if live {
                k.c.success.icon
            } else {
                k.c.text_faint
            },
        ))
        .child(
            text(hub_label(language, &s.hub_label))
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        );
    let theme_name = crate::theme::apply::current_title()
        .map(|(name, _)| name)
        .unwrap_or_else(|| s.cfg.appearance.theme_id.clone());
    let repo = env!("CARGO_PKG_REPOSITORY");
    vec![
        hero(s, k),
        group(
            k,
            "about-status",
            tr_ref(language, "This window", "当前窗口"),
            vec![
                row(
                    k,
                    "hub-mode",
                    tr_ref(language, "Connection", "连接"),
                    match s.hub_label.as_str() {
                        "local gateway" => tr_ref(
                            language,
                            "Sessions come from the Muqun gateway on this computer; your phone sees the same ones.",
                            "会话来自这台电脑上的 Muqun Gateway，手机看到的是同一批。",
                        ),
                        "standalone" => tr_ref(
                            language,
                            "No gateway is running; this window talks to the agents directly.",
                            "没有运行中的 Gateway，此窗口直接连接各 Agent。",
                        ),
                        _ => tr_ref(language, "How this window reaches the agents.", "当前窗口连接 Agents 的方式。"),
                    },
                    hub,
                ),
                row(
                    k,
                    "theme-about",
                    tr_ref(language, "Theme", "主题"),
                    "",
                    text(theme_name).font(k.ty.ui).color(k.c.text_base),
                ),
            ],
        ),
        group(
            k,
            "about-links",
            tr_ref(language, "Links", "链接"),
            vec![
                row(
                    k,
                    "link-site",
                    tr_ref(language, "Website", "官网"),
                    "muqun.dev",
                    link_button(k, "about-site", tr_ref(language, "Open", "打开"), "https://muqun.dev"),
                ),
                row(
                    k,
                    "link-themes",
                    tr_ref(language, "Theme gallery", "主题图库"),
                    "muqun.dev/themes",
                    link_button(
                        k,
                        "about-themes",
                        tr_ref(language, "Open", "打开"),
                        "https://muqun.dev/themes",
                    ),
                ),
                row(
                    k,
                    "link-repo",
                    tr_ref(language, "Source code", "源码"),
                    repo.trim_start_matches("https://"),
                    link_button(k, "about-repo", tr_ref(language, "Open", "打开"), repo),
                ),
                row(
                    k,
                    "link-issues",
                    tr_ref(language, "Report an issue", "反馈问题"),
                    tr_ref(
                        language,
                        "Tell us what went wrong, or what you'd like.",
                        "告诉我们哪里出了问题，或你想要什么。",
                    ),
                    link_button(
                        k,
                        "about-issues",
                        tr_ref(language, "Open", "打开"),
                        concat!(env!("CARGO_PKG_REPOSITORY"), "/issues"),
                    ),
                ),
            ],
        ),
        group(
            k,
            "about-licenses",
            tr_ref(language, "Open source", "开源许可"),
            vec![
                license_row(
                    k,
                    "muqun-desktop",
                    "Apache-2.0",
                    tr_ref(
                        language,
                        "This app, and the gateway it embeds.",
                        "本应用及其内嵌的 Gateway。",
                    ),
                ),
                license_row(
                    k,
                    "Rune, rune-kit, rune-fx",
                    "MIT",
                    tr_ref(
                        language,
                        "The UI framework, components and GPU effects.",
                        "UI 框架、组件与 GPU 特效。",
                    ),
                ),
                license_row(
                    k,
                    "OpenCC data",
                    "Apache-2.0",
                    tr_ref(
                        language,
                        "The Simplified to Traditional Chinese table.",
                        "简繁转换字表。",
                    ),
                ),
                license_row(
                    k,
                    tr_ref(language, "Built-in themes", "内置主题"),
                    "CC0-1.0",
                    tr_ref(
                        language,
                        "Six theme packs and their artwork.",
                        "六套内置主题及其插图。",
                    ),
                ),
                license_row(
                    k,
                    "JetBrains Mono, Noto Sans CJK",
                    "OFL-1.1",
                    tr_ref(
                        language,
                        "Fonts the UI asks the system for; not bundled.",
                        "UI 向系统请求的字体，未打包。",
                    ),
                ),
            ],
        ),
        group(
            k,
            "about-paths",
            tr_ref(language, "Files", "文件位置"),
            vec![
                path_row(
                    k,
                    "config",
                    tr_ref(language, "Settings", "设置"),
                    config.display().to_string(),
                ),
                path_row(
                    k,
                    "state",
                    tr_ref(language, "Sessions and cache", "会话与缓存"),
                    state.display().to_string(),
                ),
            ],
        ),
    ]
}
