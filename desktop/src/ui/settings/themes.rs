//! Settings → Themes: the themes on this computer and the published ones,
//! as a grid of pictures one search filters, and the ways to import one.
//!
//! The current theme sits on a faint step of the surface toward the accent,
//! with an In use badge, not outlined or lit; a theme without a picture shows
//! its light and dark colours as two small windows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rune::{col, el, row as hrow, text, El, ImageCache, SurfaceFit, TextEdit, Transition, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit,
    SegmentedControl, TextInput,
};

use super::{row, status, Tone};
use crate::config::{ConfigEdit, Language};
use crate::model::commands::Command;
use crate::theme::gallery::IndexEntry;
use crate::theme::service::Job;
use crate::theme::store::{Swatch, ThemeEntry};
use crate::ui::copy::{tr, tr_ref};
use crate::ui::UiState;

/// The narrowest a tile gets before the grid drops a column.
const TILE_MIN_W: f32 = 200.0;
const GAP: f32 = 18.0;

/// A thumbnail's bytes, read once per path: the view runs every frame and
/// the picture cache hashes what it is given.
fn thumb_bytes(path: &Path) -> Option<Arc<[u8]>> {
    thread_local! {
        static READ: std::cell::RefCell<HashMap<PathBuf, Arc<[u8]>>> = Default::default();
    }
    READ.with_borrow_mut(|read| {
        if let Some(bytes) = read.get(path) {
            return Some(bytes.clone());
        }
        let bytes: Arc<[u8]> = std::fs::read(path).ok()?.into();
        read.insert(path.to_path_buf(), bytes.clone());
        Some(bytes)
    })
}

fn hex(k: &Kit, hex: &str) -> rune::Color {
    rune::color::hex(hex).unwrap_or(k.c.layer_02)
}

/// A variant's colours as a tiny window: its floor, a surface with a line
/// of text and an accent.
fn mini_window(k: &Kit, key: &str, sw: &Swatch, w: f32, h: f32) -> El {
    let text_c = hex(k, &sw.text);
    let line = |key: &str, w: f32| {
        el().key(key.to_string())
            .w(w)
            .h((h * 0.07).max(2.0))
            .radius(1.0)
            .fill(text_c.alpha(0.55))
    };
    el().key(key.to_string())
        .w(w)
        .h(h)
        .fill(hex(k, &sw.background))
        .child(
            col()
                .key("sheet")
                .pin(Some(w * 0.12), Some(h * 0.18), Some(w * 0.12), Some(0.0))
                .radius((h * 0.08).max(2.0))
                .fill(hex(k, &sw.surface))
                .px(w * 0.08)
                .pt(h * 0.12)
                .gap(h * 0.07)
                .child(
                    el().key("accent")
                        .w(w * 0.28)
                        .h((h * 0.09).max(2.0))
                        .radius(2.0)
                        .fill(hex(k, &sw.primary)),
                )
                .child(line("l1", w * 0.5))
                .child(line("l2", w * 0.36)),
        )
        .a11y_hidden()
}

/// A theme's two variants as small windows side by side.
fn scheme_chips(k: &Kit, light: &Swatch, dark: &Swatch) -> El {
    hrow()
        .key("schemes")
        .gap(4.0)
        .child(
            mini_window(k, "light", light, 26.0, 18.0)
                .radius(4.0)
                .clip_rounded(4.0),
        )
        .child(
            mini_window(k, "dark", dark, 26.0, 18.0)
                .radius(4.0)
                .clip_rounded(4.0),
        )
        .a11y_hidden()
}

/// A tile's picture: the thumbnail, else the two variants split down the
/// middle, else a quiet well.
fn picture(
    k: &Kit,
    images: Option<&ImageCache>,
    path: Option<&Path>,
    swatches: Option<(&Swatch, &Swatch)>,
    w: f32,
) -> El {
    let h = (w / 1.6).round();
    if let (Some(images), Some(bytes)) = (images, path.and_then(thumb_bytes)) {
        let image = crate::ui::art::image(images, &bytes);
        return el()
            .key("thumb")
            .w(w)
            .h(h)
            .pixels_fit(image.pixels(), SurfaceFit::Fill);
    }
    if let Some((light, dark)) = swatches {
        return hrow()
            .key("thumb")
            .w(w)
            .h(h)
            .child(mini_window(k, "light", light, w / 2.0, h))
            .child(mini_window(k, "dark", dark, w / 2.0, h));
    }
    // Still coming (or none published): a quiet well with the themes glyph.
    el().key("thumb")
        .w(w)
        .h(h)
        .fill(k.c.text_base.alpha(0.05))
        .center()
        .child(rune::icon(&crate::ui::icons::GRID, 28.0).color(k.c.icon_faint))
}

/// What pressing a library tile does.
type Apply = Box<dyn Fn(&mut UiState)>;

struct Tile {
    key: String,
    name: String,
    about: String,
    picture: El,
    chips: Option<El>,
    trailing: Option<El>,
    status: Option<El>,
    current: bool,
    apply: Option<Apply>,
}

fn tile(s: &UiState, k: &Kit, t: Tile, w: f32) -> El {
    let look = s.look();
    let radius = k.radius.lg;
    let mut pic = el()
        .key("frame")
        .w(w)
        .radius(radius)
        .clip_rounded(radius)
        .child(t.picture);
    if t.current {
        pic = pic.child(
            el().key("current")
                .pin(None, Some(8.0), Some(8.0), None)
                .child(
                    Badge::new(tr_ref(s.cfg.general.language, "In use", "使用中"))
                        .variant(BadgeVariant::Accent)
                        .build(k),
                ),
        );
    }
    let pic = pic.box_shadow(rune::Shadow {
        x: 0.0,
        y: 4.0,
        blur: 14.0,
        spread: 0.0,
        color: k.c.black.alpha(if s.dark { 0.35 } else { 0.10 }),
        inset: false,
    });
    let pic = match t.apply {
        Some(apply) => look
            .pressable(k, pic)
            .role("button")
            .label(format!(
                "{} {}",
                tr_ref(s.cfg.general.language, "Apply", "应用"),
                t.name
            ))
            .on_press(move |s: &mut UiState| apply(s)),
        None => pic,
    };
    let words = col()
        .key("words")
        .grow(1.0)
        .shrink()
        .gap(1.0)
        .child(
            text(t.name)
                .font(k.ty.ui_medium)
                .color(if t.current {
                    k.c.text_accent
                } else {
                    k.c.text_base
                })
                .truncate(),
        )
        .child(
            text(t.about)
                .font(k.ty.small)
                .color(k.c.text_faint)
                .truncate(),
        );
    // The theme in use sits on a faint step of the surface toward the
    // accent, a little wider than the tile: no rim, no light.
    let backing = t.current.then(|| {
        el().key("current-backing")
            .pin(Some(-8.0), Some(-8.0), Some(-8.0), Some(-8.0))
            .radius(radius + 6.0)
            .fill(look.emphasis(look.panel_fill(k), k.c.bg_accent))
            .a11y_hidden()
    });
    col()
        .key(t.key)
        .w(w)
        .gap(10.0)
        .children(backing)
        .transition(
            Transition::opacity()
                .combined(Transition::offset(0.0, 10.0))
                .combined(Transition::scale(0.98)),
        )
        .child(pic)
        .child(
            hrow()
                .key("meta")
                .w_full()
                .gap(8.0)
                .center_y()
                .child(words)
                .children(t.chips)
                .children(t.trailing),
        )
        .children(t.status)
}

fn matches(query: &str, fields: &[Option<&str>]) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || fields
            .iter()
            .flatten()
            .any(|f| f.to_lowercase().contains(&q))
}

fn library_tile(s: &UiState, k: &Kit, entry: &ThemeEntry, w: f32) -> El {
    let language = s.cfg.general.language;
    let current = entry.id == s.cfg.appearance.theme_id;
    let mut about = vec![format!("v{}", entry.version)];
    about.extend(entry.author.clone());
    let scheme = s.cfg.appearance.scheme;
    let (apply_id, remove_id) = (entry.id.clone(), entry.id.clone());
    let remove = entry.origin.removable().then(|| {
        IconButton::new(&crate::ui::icons::DELETE)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(tr_ref(language, "Remove", "移除"))
            .help(tr(
                language,
                &format!("Remove {}", entry.name),
                &format!("移除 {}", entry.name),
            ))
            .on_press(move |s: &mut UiState| s.send(Command::ThemeRemove(remove_id.clone())))
            .build(k)
            .key(format!("remove-{}", entry.id))
    });
    tile(
        s,
        k,
        Tile {
            key: format!("theme-{}", entry.id),
            name: entry.name.clone(),
            about: about.join(" · "),
            picture: picture(
                k,
                s.images.as_ref(),
                entry.thumbnail.as_deref(),
                Some((&entry.light, &entry.dark)),
                w,
            ),
            chips: Some(scheme_chips(k, &entry.light, &entry.dark)),
            trailing: remove,
            status: None,
            current,
            apply: (!current).then(|| {
                Box::new(move |s: &mut UiState| {
                    let comp = s.current_computer().to_string();
                    s.edit(ConfigEdit::ComputerTheme {
                        computer: comp,
                        theme_id: apply_id.clone(),
                    });
                    s.send(Command::ThemeApply(apply_id.clone(), scheme))
                }) as Apply
            }),
        },
        w,
    )
}

fn job_label(language: Language, job: &Job) -> (Tone, String) {
    match job {
        Job::Downloading { read, total } if *total > 0 => (
            Tone::Quiet,
            tr(
                language,
                &format!("Downloading {}%", read * 100 / total),
                &format!("下载中 {}%", read * 100 / total),
            ),
        ),
        Job::Downloading { .. } => (Tone::Quiet, tr(language, "Downloading…", "下载中…")),
        Job::Verifying => (Tone::Quiet, tr(language, "Verifying…", "校验中…")),
        Job::Installing => (Tone::Quiet, tr(language, "Installing…", "安装中…")),
        Job::Done { verified: true } => (
            Tone::Good,
            tr(language, "Installed, checksum verified", "已安装，校验通过"),
        ),
        Job::Done { verified: false } => (Tone::Good, tr(language, "Installed", "已安装")),
        Job::Failed { message } => (
            Tone::Bad,
            tr(
                language,
                &format!("Failed: {message}"),
                &format!("失败：{message}"),
            ),
        ),
    }
}

fn gallery_tile(s: &UiState, k: &Kit, entry: &IndexEntry, w: f32) -> El {
    let language = s.cfg.general.language;
    let installed = s.theme_library.iter().any(|t| t.id == entry.id);
    let job = s.theme_jobs.get(&entry.id);
    let running = job.is_some_and(|j| {
        matches!(
            j,
            Job::Downloading { .. } | Job::Verifying | Job::Installing
        )
    });
    let mut about = vec![format!("v{}", entry.version)];
    about.extend(entry.author.clone());
    about.push(format!("{} KB", entry.bytes / 1024));
    let id = entry.id.clone();
    let install = Button::new(if installed {
        tr_ref(language, "Reinstall", "重新安装")
    } else {
        tr_ref(language, "Install", "安装")
    })
    .variant(if installed {
        ButtonVariant::Ghost
    } else {
        ButtonVariant::Outline
    })
    .control_size(ControlSize::Small)
    .disabled(running)
    .on_press(move |s: &mut UiState| s.send(Command::ThemeInstall(id.clone())))
    .build(k)
    .key(format!("install-{}", entry.id));
    tile(
        s,
        k,
        Tile {
            key: format!("gallery-{}", entry.id),
            name: entry.name.clone(),
            about: about.join(" · "),
            picture: picture(
                k,
                s.images.as_ref(),
                s.theme_previews.get(&entry.id).map(PathBuf::as_path),
                None,
                w,
            ),
            chips: None,
            trailing: Some(install),
            status: job.map(|job| {
                let (tone, words) = job_label(language, job);
                status(k, "job", tone, words)
            }),
            current: false,
            apply: None,
        },
        w,
    )
}

/// Tiles in equal columns filling the body's width.
fn grid(key: &str, tiles: Vec<El>, columns: usize) -> El {
    let mut grid = col().key(key.to_string()).w_full().gap(GAP + 6.0);
    let mut tiles = tiles.into_iter().peekable();
    let mut r = 0;
    while tiles.peek().is_some() {
        let mut line = hrow()
            .key(format!("line-{r}"))
            .w_full()
            .gap(GAP)
            .items_start();
        for _ in 0..columns {
            if let Some(t) = tiles.next() {
                line = line.child(t);
            }
        }
        grid = grid.child(line);
        r += 1;
    }
    grid
}

/// The page's own actions: the search, and look again.
pub(super) fn actions(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let mut search = TextInput::new(&s.theme_query)
        .label(tr_ref(language, "Search themes", "搜索主题"))
        .placeholder(tr_ref(language, "Search themes", "搜索主题"))
        .leading_icon(&rune_kit::glyphs::SEARCH)
        .leading_icon_size(14.0)
        .width(Some(220.0))
        .on_edit(|s: &mut UiState| &mut s.theme_query);
    if !s.theme_query.text.is_empty() {
        search = search.clear_button();
    }
    vec![
        search.build(k).key("theme-search"),
        IconButton::new(&crate::ui::icons::REFRESH)
            .variant(IconButtonVariant::Ghost)
            .control_size(super::CONTROL_SIZE)
            .label(tr_ref(language, "Refresh", "刷新"))
            .help(tr_ref(language, "Look for new themes", "查找新主题"))
            .on_press(|s: &mut UiState| s.send(Command::ThemeRefresh))
            .build(k)
            .key("theme-refresh"),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeTab {
    #[default]
    Installed,
    Gallery,
}

pub(super) fn sub_header(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let query = s.theme_query.text.as_str();

    let library_count = s
        .theme_library
        .iter()
        .filter(|t| {
            matches(
                query,
                &[
                    Some(&t.name),
                    Some(&t.id),
                    t.author.as_deref(),
                    t.description.as_deref(),
                    Some(&t.tags.join(" ")),
                ],
            )
        })
        .count();

    let gallery = &s.theme_gallery;
    let gallery_count = gallery
        .entries
        .iter()
        .filter(|e| {
            matches(
                query,
                &[
                    Some(&e.name),
                    Some(&e.id),
                    e.author.as_deref(),
                    e.description.as_deref(),
                    Some(&e.tags.join(" ")),
                ],
            )
        })
        .count();

    let at = match s.theme_tab {
        ThemeTab::Installed => 0,
        ThemeTab::Gallery => 1,
    };

    let gallery_label = if gallery.loading && gallery.entries.is_empty() {
        tr_ref(language, "Gallery", "图库").to_string()
    } else {
        format!("{} ({gallery_count})", tr_ref(language, "Gallery", "图库"))
    };

    let labels = [
        format!(
            "{} ({library_count})",
            tr_ref(language, "On this computer", "本机主题")
        ),
        gallery_label,
    ];

    let seg = SegmentedControl::new(&labels, Some(at))
        .fit()
        .label(tr_ref(language, "Theme sources", "主题来源"))
        .on_pick(|s: &mut UiState, i, _: &mut Tx| {
            s.theme_tab = match i {
                0 => ThemeTab::Installed,
                _ => {
                    if s.theme_gallery.entries.is_empty() && !s.theme_gallery.loading {
                        s.send(Command::ThemeRefresh);
                    }
                    ThemeTab::Gallery
                }
            };
        })
        .build(k)
        .key("theme-tabs-segmented");

    let mut bar = hrow().key("theme-tabs-bar").gap(14.0).center_y().child(seg);

    if s.theme_tab == ThemeTab::Gallery {
        bar = bar.child(
            text(tr(language, "from muqun.dev", "来自 muqun.dev"))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }

    bar
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let query = s.theme_query.text.as_str();
    // Columns from the body's width as last laid out.
    let body_w = rune::memo::layout_with("settings-body-Themes", |r| r[2] - 2.0 * super::PAD_X)
        .unwrap_or(super::BODY_MAX - 2.0 * super::PAD_X)
        .max(TILE_MIN_W);
    let columns = (((body_w + GAP) / (TILE_MIN_W + GAP)).floor() as usize).clamp(1, 7);
    let w = ((body_w - GAP * (columns - 1) as f32) / columns as f32).floor();

    let mut out = Vec::new();

    match s.theme_tab {
        ThemeTab::Installed => {
            let library: Vec<&ThemeEntry> = s
                .theme_library
                .iter()
                .filter(|t| {
                    matches(
                        query,
                        &[
                            Some(&t.name),
                            Some(&t.id),
                            t.author.as_deref(),
                            t.description.as_deref(),
                            Some(&t.tags.join(" ")),
                        ],
                    )
                })
                .collect();

            if s.theme_library.is_empty() {
                out.push(super::empty(
                    k,
                    "library-empty",
                    tr_ref(language, "No themes yet", "还没有主题"),
                    tr_ref(
                        language,
                        "Install one from the Gallery tab, or import a package.",
                        "从“图库”标签页安装，或导入主题包。",
                    ),
                ));
            } else if library.is_empty() {
                out.push(nothing_found(k, language));
            } else {
                out.push(el().key("library-pad").w_full().pt(6.0).child(grid(
                    "library-grid",
                    library.iter().map(|t| library_tile(s, k, t, w)).collect(),
                    columns,
                )));
            }

            let import = tr_ref(language, "Import", "导入");
            out.push(super::group(
                k,
                "themes-import",
                tr_ref(language, "Import", "导入"),
                vec![
                    row(
                        k,
                        "import-link",
                        tr_ref(language, "From a link", "从链接导入"),
                        tr_ref(
                            language,
                            "A .muqun-theme address someone shared.",
                            "别人分享的 .muqun-theme 地址。",
                        ),
                        super::field_button(
                            TextInput::new(&s.theme_url)
                                .label(tr_ref(language, "Theme link", "主题链接"))
                                .placeholder("https://… .muqun-theme")
                                .on_edit(|s: &mut UiState| &mut s.theme_url)
                                .on_submit(import_url),
                            &s.theme_url,
                            k,
                            "theme-url",
                            super::button(import)
                                .disabled(s.theme_url.text.trim().is_empty())
                                .on_press(|s: &mut UiState| import_url(s, &mut Tx::default())),
                            super::button_w(k, import),
                        ),
                    ),
                    row(
                        k,
                        "import-file",
                        tr_ref(language, "From a file", "从文件导入"),
                        tr_ref(
                            language,
                            "A .muqun-theme package on this computer.",
                            "这台电脑上的 .muqun-theme 主题包。",
                        ),
                        super::button(tr_ref(language, "Choose file…", "选择文件…"))
                            .on_press(|_: &mut UiState| {
                                crate::platform::dialogs::import_theme_file()
                            })
                            .build(k)
                            .key("theme-import-file"),
                    ),
                ],
            ));
        }
        ThemeTab::Gallery => {
            let gallery = &s.theme_gallery;
            let found: Vec<&IndexEntry> = gallery
                .entries
                .iter()
                .filter(|e| {
                    matches(
                        query,
                        &[
                            Some(&e.name),
                            Some(&e.id),
                            e.author.as_deref(),
                            e.description.as_deref(),
                            Some(&e.tags.join(" ")),
                        ],
                    )
                })
                .collect();

            if gallery.loading && gallery.entries.is_empty() {
                out.push(el().key("gallery-loading").py(12.0).child(status(
                    k,
                    "loading",
                    Tone::Quiet,
                    tr(language, "Loading the gallery…", "正在加载图库…"),
                )));
            }
            if let Some(err) = &gallery.error {
                out.push(row(
                    k,
                    "gallery-error",
                    tr_ref(language, "The gallery is unavailable", "图库不可用"),
                    err,
                    super::button(tr_ref(language, "Try again", "重试"))
                        .on_press(|s: &mut UiState| s.send(Command::ThemeRefresh))
                        .build(k)
                        .key("gallery-retry"),
                ));
            }
            if let Some(why) = &gallery.stale {
                out.push(el().key("gallery-stale").py(10.0).child(status(
                    k,
                    "stale",
                    Tone::Quiet,
                    tr(
                        language,
                        &format!("Showing the saved list: {why}"),
                        &format!("显示已保存的列表：{why}"),
                    ),
                )));
            }
            if !found.is_empty() {
                let mut block = col()
                    .key("gallery-pad")
                    .w_full()
                    .pt(6.0)
                    .gap(18.0)
                    .child(grid(
                        "gallery-grid",
                        found
                            .iter()
                            .take(s.gallery_limit)
                            .map(|e| gallery_tile(s, k, e, w))
                            .collect(),
                        columns,
                    ));
                if found.len() > s.gallery_limit {
                    block = block.child(
                        hrow().key("more").w_full().justify_center().child(
                            Button::new(tr_ref(language, "Show more", "显示更多"))
                                .variant(ButtonVariant::Ghost)
                                .control_size(ControlSize::Regular)
                                .on_press(|s: &mut UiState| {
                                    s.gallery_limit += super::super::GALLERY_PAGE
                                })
                                .build(k)
                                .key("gallery-more"),
                        ),
                    );
                }
                out.push(block);
            } else if !gallery.entries.is_empty() {
                out.push(nothing_found(k, language));
            }
        }
    }

    out
}

fn nothing_found(k: &Kit, language: Language) -> El {
    el().key("nothing-found").py(12.0).child(status(
        k,
        "none",
        Tone::Quiet,
        tr(
            language,
            "No theme matches the search.",
            "没有符合搜索的主题。",
        ),
    ))
}

fn import_url(s: &mut UiState, _: &mut Tx) {
    let url = s.theme_url.text.trim().to_string();
    if url.is_empty() {
        return;
    }
    s.theme_url = TextEdit::new("");
    s.send(Command::ThemeImportUrl(url));
}
