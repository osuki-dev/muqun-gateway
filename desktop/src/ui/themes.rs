//! The themes panel (architecture §5, §6.1d): what is installed, what the
//! gallery offers, and importing a `.muqun-theme` from disk or a public HTTPS
//! package link (App link import, package URL — not a git repo).
//!
//! Mounted from Settings → Appearance ([`themes_panel`]). Ctrl+Shift+T still
//! opens the same panel as an overlay ([`overlay`]).
//!
//! Every action is a `Command::Theme*` (`theme/service.rs`); results come
//! back as `ThemeUpdate`s, written into the keyed states below by
//! [`apply_update`] on the main thread. A card's picture and its install
//! progress are their own keyed states, so a download's progress or a
//! preview arriving rebuilds that card's part and nothing else.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use blinc_cn::prelude::{button, input, ButtonSize, ButtonVariant};
use blinc_core::{BlincContextState, Color, State};
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_input::{text_input_data_with_placeholder, SharedTextInputData};

use super::text::ui_text;
use super::App;
use crate::config::{Language, Scheme};
use crate::model::commands::Command;
use crate::theme::gallery::IndexEntry;
use crate::theme::service::{GalleryView, Job, Notice, ThemeUpdate};
use crate::theme::store::{Origin, Swatch, ThemeEntry};

/// The panel's own stylesheet; added once with the app's (`install_css`).
pub const THEMES_CSS: &str = include_str!("themes.css");

const MIN_CARD: f32 = 232.0;
const GAP: f32 = 16.0;
const PAD_X: f32 = 24.0;
/// Matches `--mq-card-pad` in themes.css.
const CARD_PAD: f32 = 20.0;
/// Matches `--mq-radius-md` (preview inside `--mq-radius-lg` card).
const RADIUS_MD: f32 = 8.0;
/// One row of `.mq-theme-tag` pills (padding + 11px type).
const TAGS_H: f32 = 22.0;
/// Small-button action row; status truncates inside this height.
const ACTIONS_H: f32 = 28.0;
/// Standalone overlay: title + scheme + import, then tabs/search, then link.
const HEADER_H: f32 = 148.0;
/// Settings → Appearance: no title/scheme row (Reading owns those); tabs + link only.
const HEADER_H_EMBEDDED: f32 = 88.0;
/// Gallery cards built at once. A theme switch rebuilds the whole tree, and
/// every card is a picture: all 97 made one switch cost up to a second.
const GALLERY_PAGE: usize = 24;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Installed,
    Gallery,
}

/// The panel's keyed states. Cheap to fetch; the keys are fixed.
#[derive(Clone)]
pub struct ThemeBindings {
    pub library: State<Arc<Vec<ThemeEntry>>>,
    pub gallery: State<Arc<GalleryView>>,
    pub current: State<Option<(String, Scheme)>>,
    pub tab: State<Tab>,
    pub query: State<String>,
    pub notice: State<Option<Notice>>,
    pub open: State<bool>,
    /// `!open`, for the shell body's `visible()` (it takes a state, not a
    /// negation).
    pub closed: State<bool>,
    /// The keyboard-selected card, by position in the shown list.
    pub selected: State<usize>,
    /// How many gallery cards are built ([`GALLERY_PAGE`] more per "Show more").
    pub limit: State<usize>,
    pub search: SharedTextInputData,
    /// HTTPS `.muqun-theme` package link (App link import, package URL only).
    pub link: SharedTextInputData,
}

impl ThemeBindings {
    pub fn get() -> Self {
        let ctx = BlincContextState::get();
        Self {
            library: ctx.use_state_keyed("mq.themes.library", || Arc::new(Vec::new())),
            gallery: ctx.use_state_keyed("mq.themes.gallery", || Arc::new(GalleryView::default())),
            current: ctx.use_state_keyed("mq.themes.current", crate::theme::apply::current_choice),
            tab: ctx.use_state_keyed("mq.themes.tab", || Tab::Installed),
            query: ctx.use_state_keyed("mq.themes.query", String::new),
            notice: ctx.use_state_keyed("mq.themes.notice", || None),
            open: ctx.use_state_keyed("mq.themes.open", || false),
            closed: ctx.use_state_keyed("mq.themes.closed", || true),
            selected: ctx.use_state_keyed("mq.themes.selected", || 0),
            limit: ctx.use_state_keyed("mq.themes.limit", || GALLERY_PAGE),
            search: ctx
                .use_state_keyed("mq.themes.search", || {
                    text_input_data_with_placeholder("Search name or tag")
                })
                .get(),
            link: ctx
                .use_state_keyed("mq.themes.link", || {
                    text_input_data_with_placeholder("https://…/theme.muqun-theme")
                })
                .get(),
        }
    }
}

fn job_state(id: &str) -> State<Option<Job>> {
    BlincContextState::get().use_state_keyed(&format!("mq.themes.job.{id}"), || None)
}

fn preview_state(id: &str) -> State<Option<PathBuf>> {
    BlincContextState::get().use_state_keyed(&format!("mq.themes.preview.{id}"), || None)
}

/// Apply one update from the tokio side. Main thread (the drain).
pub fn apply_update(update: ThemeUpdate) {
    let b = ThemeBindings::get();
    match update {
        ThemeUpdate::Library(list) => b.library.signal().set(list),
        ThemeUpdate::Gallery(view) => b.gallery.signal().set(view),
        ThemeUpdate::Preview { id, path } => preview_state(&id).signal().set(Some(path)),
        ThemeUpdate::Job { id, job } => job_state(&id).signal().set(Some(job)),
        ThemeUpdate::Applied(applied) => {
            let choice = (applied.manifest.id.clone(), applied.scheme);
            match crate::theme::apply::activate(applied) {
                Ok(_) => b.current.signal().set(Some(choice)),
                Err(err) => b.notice.signal().set(Some(Notice {
                    message: format!("Could not apply {}: {err:#}", choice.0),
                    error: true,
                    retry: None,
                })),
            }
        }
        ThemeUpdate::Notice(notice) => b.notice.signal().set(Some(notice)),
    }
}

/// English or Chinese, by the app's language.
fn tr(app: &App, en: &'static str, zh: &'static str) -> &'static str {
    match app.strings.language() {
        Language::ZhCn => zh,
        Language::En => en,
    }
}

/// Ask for the library and the gallery once per run; the panel is rebuilt
/// on every theme switch and must not re-ask each time.
pub(crate) fn refresh_once(app: &App) {
    static ASKED: AtomicBool = AtomicBool::new(false);
    if !ASKED.swap(true, Ordering::Relaxed) {
        app.send(Command::ThemeRefresh);
    }
}

/// Files dropped on the window: every `.muqun-theme` among them is
/// imported. Process-wide (Blinc keeps one drop handler), so it is set once.
///
/// Themes share the window drop handler with the composer (`.muqun-theme`
/// imports; other files stage as attachments).
pub fn install_drop_handler(app: &App) {
    super::composer::install_file_drop(app);
}

/// The system file dialog for a `.muqun-theme`. It blocks its caller: on
/// macOS it must run on the main thread, elsewhere beside the UI.
fn pick_theme_file(app: &App) {
    let commands = app.commands.clone();
    let title = tr(app, "Import a theme", "导入主题");
    let pick = move || {
        let picked = blinc_app::dialog::open_file()
            .title(title)
            .filter(blinc_app::dialog::FileFilter::new("Muqun theme").ext("muqun-theme"))
            .pick();
        if let Some(path) = picked {
            crate::model::commands::send(&commands, Command::ThemeImportFile(path));
        }
    };
    if crate::platform::shell::dialogs_need_main_thread() {
        pick();
    } else {
        std::thread::spawn(pick);
    }
}

/// Back to the first card and the first gallery page (a new tab or query).
fn reset_paging() {
    let b = ThemeBindings::get();
    blinc_core::reactive::batch_stateful_deps(|| {
        b.selected.signal().set(0);
        b.limit.signal().set(GALLERY_PAGE);
    });
}

/// Open or close the overlay.
pub fn toggle() {
    set_open(!ThemeBindings::get().open.get());
}

fn set_open(open: bool) {
    let b = ThemeBindings::get();
    blinc_core::reactive::batch_stateful_deps(|| {
        b.open.signal().set(open);
        b.closed.signal().set(!open);
    });
}

/// The shell body, hidden (not rebuilt) while the panel stands in for it.
pub fn shell_body(body: Div) -> Div {
    div()
        .w_full()
        .visible(ThemeBindings::get().closed)
        .child(body)
}

/// The panel in place of the shell body, while open (Ctrl+Shift+T). A
/// stand-in for the Settings → Appearance entry point.
pub fn overlay(app: &App, width: f32, height: f32) -> impl ElementBuilder {
    let open = ThemeBindings::get().open;
    let app = app.clone();
    stateful_with_key::<NoState>("mq-themes-overlay")
        .deps([open.signal_id()])
        .on_state(move |_| {
            if !open.get() {
                return div();
            }
            let close = button(tr(&app, "Close", "关闭"))
                .variant(ButtonVariant::Ghost)
                .size(ButtonSize::Small)
                .on_click(move |_| set_open(false));
            div()
                .class("mq-themes-overlay")
                .w(width)
                .h(height)
                .flex_col()
                .child(
                    div()
                        .class("mq-themes-overlay-bar")
                        .w_full()
                        .h(44.0)
                        .flex_row()
                        .items_center()
                        .child(
                            div().flex_1().child(
                                ui_text(tr(&app, "Appearance", "外观"))
                                    .size(13.0)
                                    .semibold(),
                            ),
                        )
                        .child(close),
                )
                .child(themes_panel(&app, width, (height - 44.0).max(0.0)))
        })
}

/// The themes panel: Installed / Gallery, search, import.
pub fn themes_panel(app: &App, width: f32, height: f32) -> Div {
    panel(app, width, height, false)
}

/// Settings → Appearance already pads the tab; skip the panel's own inset
/// so the library lines up with Reading.
pub fn themes_panel_embedded(app: &App, width: f32, height: f32) -> Div {
    panel(app, width, height, true)
}

fn panel(app: &App, width: f32, height: f32, embedded: bool) -> Div {
    refresh_once(app);
    install_drop_handler(app);
    let b = ThemeBindings::get();
    let pad_x = if embedded { 0.0 } else { PAD_X };
    let header_h = if embedded {
        HEADER_H_EMBEDDED
    } else {
        HEADER_H
    };
    let mut root = div().class("mq-themes");
    if embedded {
        root = root.class("mq-themes--embedded");
    }
    root.relative()
        .w(width)
        .h(height)
        .flex_col()
        .child(header(app, &b, header_h, embedded))
        .child(grid(
            app,
            &b,
            width,
            (height - header_h).max(0.0),
            pad_x,
        ))
        .child(notice_strip(app, &b, pad_x))
}

fn header(app: &App, b: &ThemeBindings, header_h: f32, embedded: bool) -> impl ElementBuilder {
    let (current, tab, library, gallery) = (
        b.current.clone(),
        b.tab.clone(),
        b.library.clone(),
        b.gallery.clone(),
    );
    let search = b.search.clone();
    let link = b.link.clone();
    let query = b.query.clone();
    let app = app.clone();
    // Embedded: Appearance owns "Themes" + scheme; header is tabs/search + import only.
    let key = if embedded {
        "mq-themes-header-embedded"
    } else {
        "mq-themes-header"
    };
    stateful_with_key::<NoState>(key)
        .deps([
            current.signal_id(),
            tab.signal_id(),
            library.signal_id(),
            gallery.signal_id(),
        ])
        .on_state(move |_| {
            let import = {
                let app = app.clone();
                button(tr(&app, "Import file…", "导入文件…"))
                    .variant(ButtonVariant::Outline)
                    .size(ButtonSize::Small)
                    .on_click(move |_| pick_theme_file(&app))
            };
            let import_link = {
                let app = app.clone();
                let link = link.clone();
                button(tr(&app, "Import link", "导入链接"))
                    .variant(ButtonVariant::Outline)
                    .size(ButtonSize::Small)
                    .on_click(move |_| {
                        let url = link
                            .lock()
                            .map(|d| d.value.trim().to_string())
                            .unwrap_or_default();
                        if url.is_empty() {
                            return;
                        }
                        app.send(Command::ThemeImportUrl(url));
                    })
            };
            let active = tab.get();
            let tab_button = |which: Tab, label: String| {
                let (tab, app) = (tab.clone(), app.clone());
                let mut tab_div = div().class("mq-themes-tab");
                if which == active {
                    tab_div = tab_div.class("mq-themes-tab--active");
                }
                tab_div
                    .cursor_pointer()
                    .child(ui_text(label).size(13.0).semibold().no_cursor())
                    .on_click(move |_| {
                        tab.signal().set(which);
                        reset_paging();
                        if which == Tab::Gallery {
                            app.send(Command::ThemeRefresh);
                        }
                    })
            };
            let installed_n = library.get().len();
            let gallery_n = gallery.get().entries.len();
            let query = query.clone();
            let toolbar = div()
                .class("mq-themes-toolbar")
                .w_full()
                .flex_row()
                .items_center()
                .child(tab_button(
                    Tab::Installed,
                    format!("{} · {installed_n}", tr(&app, "Installed", "已安装")),
                ))
                .child(tab_button(
                    Tab::Gallery,
                    format!("{} · {gallery_n}", tr(&app, "Gallery", "画廊")),
                ))
                .child(div().flex_1())
                .child(div().w(240.0).child(input(&search).w_full().on_change(
                    move |text: &str| {
                        query.signal().set(text.trim().to_lowercase());
                        reset_paging();
                    },
                )));
            let link_row = div()
                .class("mq-themes-link-row")
                .w_full()
                .flex_row()
                .items_center()
                .child(div().flex_1().min_w(0.0).child(input(&link).w_full()))
                .child(import_link)
                .child(import);

            let mut head = div()
                .class("mq-themes-header")
                .w_full()
                .h(header_h)
                .flex_col()
                .flex_shrink_0();

            if !embedded {
                let choice = current.get();
                let scheme = choice.as_ref().map(|c| c.1).unwrap_or(Scheme::System);
                let mut schemes = div().class("mq-segmented").flex_row().items_center();
                for (s, label) in [
                    (Scheme::Light, tr(&app, "Light", "浅色")),
                    (Scheme::Dark, tr(&app, "Dark", "深色")),
                    (Scheme::System, tr(&app, "System", "跟随系统")),
                ] {
                    let app2 = app.clone();
                    let id = choice.as_ref().map(|c| c.0.clone());
                    schemes = schemes.child(
                        button(label)
                            .variant(if s == scheme {
                                ButtonVariant::Secondary
                            } else {
                                ButtonVariant::Ghost
                            })
                            .size(ButtonSize::Small)
                            .on_click(move |_| {
                                if let Some(id) = id.clone() {
                                    crate::theme::apply::trace(
                                        "mq.theme.click",
                                        format_args!("id={id} scheme={s:?}"),
                                    );
                                    app2.send(Command::ThemeApply(id, s));
                                }
                            }),
                    );
                }
                head = head.child(
                    div()
                        .class("mq-themes-title-row")
                        .w_full()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .child(ui_text(tr(&app, "Themes", "主题")).size(15.0).semibold()),
                        )
                        .child(schemes),
                );
            }

            head.child(toolbar).child(link_row)
        })
}

fn matches(query: &str, name: &str, id: &str, author: Option<&str>, tags: &[String]) -> bool {
    query.is_empty()
        || name.to_lowercase().contains(query)
        || id.contains(query)
        || author.is_some_and(|a| a.to_lowercase().contains(query))
        || tags.iter().any(|t| t.contains(query))
}

/// Columns and card width for `width`, after the panel's horizontal pad.
fn layout(width: f32, pad_x: f32) -> (usize, f32) {
    let inner = (width - 2.0 * pad_x).max(MIN_CARD);
    let columns = (((inner + GAP) / (MIN_CARD + GAP)).floor() as usize).max(1);
    let card = (inner - GAP * (columns as f32 - 1.0)) / columns as f32;
    (columns, card)
}

fn grid(app: &App, b: &ThemeBindings, width: f32, height: f32, pad_x: f32) -> impl ElementBuilder {
    let b = b.clone();
    let app = app.clone();
    let content = stateful_with_key::<NoState>("mq-themes-grid")
        .deps([
            b.library.signal_id(),
            b.gallery.signal_id(),
            b.current.signal_id(),
            b.tab.signal_id(),
            b.query.signal_id(),
            b.selected.signal_id(),
            b.limit.signal_id(),
        ])
        .on_state(move |_| {
            let (columns, card_w) = layout(width, pad_x);
            COLUMNS.store(columns, Ordering::Relaxed);
            let selected = b.selected.get();
            let mark = |card: Div, at: usize| {
                if at == selected {
                    card.class("mq-theme-card--selected")
                } else {
                    card
                }
            };
            let query = b.query.get();
            let current = b.current.get().map(|c| c.0);
            let library = b.library.get();
            let mut cards = div()
                .class("mq-themes-grid")
                .w_full()
                .flex_row()
                .flex_wrap()
                .items_stretch();
            let mut shown = 0;
            match b.tab.get() {
                Tab::Installed => {
                    for entry in library
                        .iter()
                        .filter(|e| matches(&query, &e.name, &e.id, e.author.as_deref(), &e.tags))
                    {
                        cards = cards.child(mark(
                            library_card(&app, entry, card_w, current.as_deref()),
                            shown,
                        ));
                        shown += 1;
                    }
                }
                Tab::Gallery => {
                    let view = b.gallery.get();
                    if let Some(error) = &view.error {
                        return div()
                            .class("mq-themes-empty")
                            .flex_col()
                            .child(
                                ui_text(tr(
                                    &app,
                                    "The gallery could not be loaded.",
                                    "画廊加载失败。",
                                ))
                                .size(15.0)
                                .semibold(),
                            )
                            .child(ui_text(error.clone()).size(13.0))
                            .child(retry_button(&app, Command::ThemeRefresh));
                    }
                    if view.loading && view.entries.is_empty() {
                        return div().class("mq-themes-empty").child(
                            ui_text(tr(&app, "Loading the gallery…", "正在加载画廊…")).size(13.0),
                        );
                    }
                    if let Some(stale) = &view.stale {
                        cards = cards.child(
                            div().class("mq-themes-stale").w_full().child(
                                ui_text(format!(
                                    "{} {stale}",
                                    tr(
                                        &app,
                                        "Showing the saved catalogue:",
                                        "显示的是本地保存的目录："
                                    )
                                ))
                                .size(12.0),
                            ),
                        );
                    }
                    let limit = b.limit.get();
                    let matching: Vec<&IndexEntry> = view
                        .entries
                        .iter()
                        .filter(|e| matches(&query, &e.name, &e.id, e.author.as_deref(), &e.tags))
                        .collect();
                    let more = matching.len().saturating_sub(limit);
                    for entry in matching.into_iter().take(limit) {
                        let installed = library
                            .iter()
                            .any(|l| l.id == entry.id && l.origin != Origin::Builtin);
                        cards = cards.child(mark(
                            gallery_card(&app, entry, card_w, installed, current.as_deref()),
                            shown,
                        ));
                        shown += 1;
                    }
                    if more > 0 {
                        let limit_state = b.limit.clone();
                        cards = cards.child(
                            div().class("mq-themes-more").w_full().child(
                                button(format!("{} ({more})", tr(&app, "Show more", "显示更多")))
                                    .variant(ButtonVariant::Outline)
                                    .size(ButtonSize::Small)
                                    .on_click(move |_| {
                                        limit_state.signal().set(limit + GALLERY_PAGE)
                                    }),
                            ),
                        );
                    }
                }
            }
            if shown == 0 && !query.is_empty() {
                cards = cards.child(
                    div().class("mq-themes-empty").child(
                        ui_text(format!(
                            "{} “{query}”",
                            tr(&app, "No theme matches", "没有匹配的主题：")
                        ))
                        .size(13.0),
                    ),
                );
            }
            div().class("mq-themes-scroll").w_full().child(cards)
        });
    div()
        .w_full()
        .h(height)
        .child(scroll().w_full().h(height).child(content))
}

/// A swatch is the previewed theme's own colour (data, not a token of the
/// running one); grey when the pack's value does not read.
fn hex(c: &str) -> Color {
    Color::from_hex_str(c).unwrap_or(Color::GRAY)
}

/// A card without a picture: the theme's own light and dark palettes.
fn swatches(light: &Swatch, dark: &Swatch, w: f32, h: f32) -> Div {
    let half = |s: &Swatch| {
        div()
            .w(w / 2.0)
            .h(h)
            .bg(hex(&s.background))
            .flex_col()
            .justify_center()
            .items_center()
            .child(
                div()
                    .flex_row()
                    .child(div().w(18.0).h(18.0).rounded(9.0).bg(hex(&s.primary)))
                    .child(div().w(18.0).h(18.0).rounded(9.0).bg(hex(&s.surface)))
                    .child(div().w(18.0).h(18.0).rounded(9.0).bg(hex(&s.text))),
            )
    };
    div()
        .class("mq-theme-card-picture")
        .w_full()
        .h(h)
        .flex_shrink_0()
        .flex_row()
        .overflow_clip()
        .child(half(light))
        .child(half(dark))
}

fn picture(path: &std::path::Path, w: f32, h: f32) -> Div {
    div()
        .class("mq-theme-card-picture")
        .w_full()
        .h(h)
        .flex_shrink_0()
        .overflow_clip()
        .child(
            image(path.to_string_lossy().to_string())
                .size(w, h)
                .cover()
                .rounded(RADIUS_MD),
        )
}

fn tags(tags: &[String]) -> Div {
    let mut row = div()
        .class("mq-theme-tags")
        .w_full()
        .h(TAGS_H)
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .overflow_clip();
    for tag in tags.iter().take(4) {
        row = row.child(
            div()
                .class("mq-theme-tag")
                .flex_shrink_0()
                .child(ui_text(tag.clone()).size(11.0).no_wrap()),
        );
    }
    row
}

fn card_shell(card_w: f32, active: bool) -> Div {
    let mut card = div()
        .class("mq-theme-card")
        .w(card_w)
        .flex_col()
        .self_stretch();
    if active {
        card = card.class("mq-theme-card--active");
    }
    card
}

fn title_block(name: &str, author: Option<&str>, meta: String) -> Div {
    let byline = match author {
        Some(a) => format!("{a} · {meta}"),
        None => meta,
    };
    div()
        .class("mq-theme-card-title")
        .w_full()
        .min_w(0.0)
        .flex_shrink_0()
        .flex_col()
        .overflow_clip()
        .child(ui_text(name.to_string()).size(15.0).semibold().no_wrap())
        .child(
            div()
                .class("mq-theme-card-meta")
                .w_full()
                .min_w(0.0)
                .overflow_clip()
                .child(ui_text(byline).size(12.0).no_wrap()),
        )
}

fn card_spacer() -> Div {
    div().class("mq-theme-card-spacer").w_full().flex_1().min_h(0.0)
}

fn actions_row() -> Div {
    div()
        .class("mq-theme-card-actions")
        .w_full()
        .h(ACTIONS_H)
        .flex_shrink_0()
        .flex_row()
        .items_center()
}

fn status_label(text: String) -> Div {
    div()
        .class("mq-theme-status")
        .flex_1()
        .min_w(0.0)
        .overflow_clip()
        .child(ui_text(text).size(12.0).no_wrap())
}

fn library_card(app: &App, entry: &ThemeEntry, card_w: f32, current: Option<&str>) -> Div {
    let inner = card_w - 2.0 * CARD_PAD;
    let pic_h = inner * 5.0 / 8.0;
    let is_current = current == Some(entry.id.as_str());
    let pic = match &entry.thumbnail {
        Some(path) => picture(path, inner, pic_h),
        None => swatches(&entry.light, &entry.dark, inner, pic_h),
    };
    let origin = match entry.origin {
        Origin::Builtin => tr(app, "Built-in", "内置"),
        Origin::Installed => tr(app, "Installed", "已安装"),
        Origin::InstalledOverBuiltin => {
            tr(app, "Installed (updates built-in)", "已安装（覆盖内置）")
        }
    };
    // Status slot left (empty here — origin lives in meta); actions pinned right.
    let mut actions = actions_row().child(div().flex_1().min_w(0.0));
    if is_current {
        actions = actions.child(
            button(tr(app, "Applied", "使用中"))
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Small)
                .disabled(true),
        );
    } else {
        let (app2, id) = (app.clone(), entry.id.clone());
        actions = actions.child(
            button(tr(app, "Apply", "应用"))
                .size(ButtonSize::Small)
                .on_click(move |_| apply(&app2, id.clone())),
        );
    }
    if entry.origin.removable() {
        let (app2, id) = (app.clone(), entry.id.clone());
        actions = actions.child(
            button(tr(app, "Remove", "移除"))
                .variant(ButtonVariant::Ghost)
                .size(ButtonSize::Small)
                .on_click(move |_| app2.send(Command::ThemeRemove(id.clone()))),
        );
    }
    card_shell(card_w, is_current)
        .child(pic)
        .child(title_block(
            &entry.name,
            entry.author.as_deref(),
            format!("v{} · {origin}", entry.version),
        ))
        .child(tags(&entry.tags))
        .child(card_spacer())
        .child(actions)
}

/// Apply `id` in the scheme currently chosen: the configuration's, which the
/// top bar's toggle and Settings change too.
fn apply(app: &App, id: String) {
    let scheme = app.config.get().appearance.scheme;
    crate::theme::apply::trace("mq.theme.click", format_args!("id={id} scheme={scheme:?}"));
    app.send(Command::ThemeApply(id, scheme));
}

fn retry_button(app: &App, command: Command) -> impl ElementBuilder {
    let app = app.clone();
    button(tr(&app, "Retry", "重试"))
        .variant(ButtonVariant::Outline)
        .size(ButtonSize::Small)
        .on_click(move |_| app.send(command.clone()))
}

fn size_text(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

fn gallery_card(
    app: &App,
    entry: &IndexEntry,
    card_w: f32,
    installed: bool,
    current: Option<&str>,
) -> Div {
    let inner = card_w - 2.0 * CARD_PAD;
    let pic_h = inner * 5.0 / 8.0;
    let preview = preview_state(&entry.id);
    let picture_part = {
        let preview = preview.clone();
        let name = entry.name.clone();
        stateful_with_key::<NoState>(format!("mq-themes-gpic-{}", entry.id))
            .deps([preview.signal_id()])
            .on_state(move |_| match preview.get() {
                Some(path) => picture(&path, inner, pic_h),
                None => div()
                    .class("mq-theme-card-picture")
                    .class("mq-theme-card-placeholder")
                    .w_full()
                    .h(pic_h)
                    .flex_shrink_0()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .child(ui_text(name.clone()).size(13.0)),
            })
    };
    let job = job_state(&entry.id);
    let action_part = {
        let (job, app, entry) = (job.clone(), app.clone(), entry.clone());
        let is_current = current == Some(entry.id.as_str());
        stateful_with_key::<NoState>(format!("mq-themes-gact-{}", entry.id))
            .deps([job.signal_id()])
            .on_state(move |_| gallery_actions(&app, &entry, job.get(), installed, is_current))
    };
    card_shell(card_w, current == Some(entry.id.as_str()))
        .child(picture_part)
        .child(title_block(
            &entry.name,
            entry.author.as_deref(),
            format!("v{} · {}", entry.version, size_text(entry.bytes)),
        ))
        .child(tags(&entry.tags))
        .child(card_spacer())
        .child(action_part)
}

fn gallery_actions(
    app: &App,
    entry: &IndexEntry,
    job: Option<Job>,
    installed: bool,
    is_current: bool,
) -> Div {
    let row = actions_row();
    let install = |label: &'static str| {
        let (app, id) = (app.clone(), entry.id.clone());
        button(label)
            .size(ButtonSize::Small)
            .on_click(move |_| app.send(Command::ThemeInstall(id.clone())))
    };
    let apply_button = || {
        let (app2, id) = (app.clone(), entry.id.clone());
        button(if is_current {
            tr(app, "Applied", "使用中")
        } else {
            tr(app, "Apply", "应用")
        })
        .variant(if is_current {
            ButtonVariant::Secondary
        } else {
            ButtonVariant::Primary
        })
        .size(ButtonSize::Small)
        .disabled(is_current)
        .on_click(move |_| apply(&app2, id.clone()))
    };
    let checksum = if entry.sha256.is_some() {
        tr(app, "SHA-256 listed", "带 SHA-256 校验")
    } else {
        tr(app, "No checksum in the catalogue", "目录未提供校验值")
    };
    match job {
        Some(Job::Downloading { read, total }) => {
            let fraction = if total > 0 {
                (read as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            const BAR_W: f32 = 56.0;
            row.child(status_label(format!(
                "{} {}% · {} / {}",
                tr(app, "Downloading", "正在下载"),
                (fraction * 100.0).round(),
                size_text(read),
                size_text(total)
            )))
            .child(
                div().class("mq-theme-progress").w(BAR_W).h(4.0).flex_shrink_0().child(
                    div()
                        .class("mq-theme-progress-bar")
                        .w(BAR_W * fraction)
                        .h(4.0),
                ),
            )
        }
        Some(Job::Verifying) => row.child(status_label(
            tr(
                app,
                "Checking SHA-256 and the package…",
                "正在校验 SHA-256 与主题包…",
            )
            .into(),
        )),
        Some(Job::Installing) => {
            row.child(status_label(tr(app, "Installing…", "正在安装…").into()))
        }
        Some(Job::Done { verified }) => row
            .child(status_label(
                if verified {
                    tr(
                        app,
                        "Installed · SHA-256 verified",
                        "已安装 · SHA-256 校验通过",
                    )
                } else {
                    tr(
                        app,
                        "Installed · not verified (no checksum)",
                        "已安装 · 未校验（目录无校验值）",
                    )
                }
                .into(),
            ))
            .child(apply_button()),
        Some(Job::Failed { message }) => row
            .child(
                div()
                    .class("mq-theme-error")
                    .flex_1()
                    .min_w(0.0)
                    .overflow_clip()
                    .child(ui_text(message).size(12.0).no_wrap()),
            )
            .child(install(tr(app, "Retry install", "重试安装"))),
        None if installed => row
            .child(status_label(tr(app, "Installed", "已安装").into()))
            .child(apply_button()),
        None => row
            .child(status_label(checksum.into()))
            .child(install(tr(app, "Install", "安装"))),
    }
}

/// Columns of the grid as last laid out, for ↑/↓.
static COLUMNS: AtomicUsize = AtomicUsize::new(1);

const KEY_ENTER: u32 = 13;
const KEY_ESCAPE: u32 = 27;
const KEY_LEFT: u32 = 37;
const KEY_UP: u32 = 38;
const KEY_RIGHT: u32 = 39;
const KEY_DOWN: u32 = 40;
const KEY_DELETE: u32 = 127;
const KEY_1: u32 = 49;
const KEY_2: u32 = 50;
const KEY_D: u32 = 68;
const KEY_L: u32 = 76;
const KEY_O: u32 = 79;
const KEY_S: u32 = 83;

/// What Enter does on a card.
enum Primary {
    Apply(String),
    Install(String),
    None,
}

/// The cards on screen, in order: what Enter does on each, and the id
/// Delete removes, if any.
fn shown(b: &ThemeBindings) -> Vec<(Primary, Option<String>)> {
    let query = b.query.get();
    let current = b.current.get().map(|c| c.0);
    let library = b.library.get();
    match b.tab.get() {
        Tab::Installed => library
            .iter()
            .filter(|e| matches(&query, &e.name, &e.id, e.author.as_deref(), &e.tags))
            .map(|e| {
                let primary = if current.as_deref() == Some(e.id.as_str()) {
                    Primary::None
                } else {
                    Primary::Apply(e.id.clone())
                };
                (primary, e.origin.removable().then(|| e.id.clone()))
            })
            .collect(),
        Tab::Gallery => b
            .gallery
            .get()
            .entries
            .iter()
            .filter(|e| matches(&query, &e.name, &e.id, e.author.as_deref(), &e.tags))
            .take(b.limit.get())
            .map(|e| {
                let job = job_state(&e.id).get();
                let installed = matches!(job, Some(Job::Done { .. }))
                    || library
                        .iter()
                        .any(|l| l.id == e.id && l.origin != Origin::Builtin);
                let busy = matches!(
                    job,
                    Some(Job::Downloading { .. } | Job::Verifying | Job::Installing)
                );
                let primary = if busy || current.as_deref() == Some(e.id.as_str()) {
                    Primary::None
                } else if installed {
                    Primary::Apply(e.id.clone())
                } else {
                    Primary::Install(e.id.clone())
                };
                (primary, None)
            })
            .collect(),
    }
}

fn set_scheme(app: &App, scheme: Scheme) {
    if let Some((id, _)) = ThemeBindings::get().current.get() {
        crate::theme::apply::trace("mq.theme.click", format_args!("id={id} scheme={scheme:?}"));
        app.send(Command::ThemeApply(id, scheme));
    }
}

/// The scheme was changed elsewhere (the top bar, Settings): the panel's
/// Light / Dark / System choice follows.
pub fn note_scheme(scheme: Scheme) {
    let b = ThemeBindings::get();
    if let Some((id, _)) = b.current.get() {
        b.current.signal().set(Some((id, scheme)));
    }
}

/// Whether the overlay is open.
pub fn is_open() -> bool {
    ThemeBindings::get().open.get()
}

/// The panel's keys (architecture §6.1c: lists move with the arrows, Enter
/// acts, Esc closes; nothing is mouse-only). True when the key was the
/// panel's.
///
/// ←/→/↑/↓ select a card · Enter applies or installs it · Delete removes
/// an installed one · Ctrl+1 / Ctrl+2 Installed / Gallery · Ctrl+O import ·
/// Ctrl+Shift+L / D / S light, dark, follow the system · Esc closes.
pub fn on_key(app: &App, ctx: &blinc_layout::event_handler::EventContext) -> bool {
    let b = ThemeBindings::get();
    let chord = ctx.ctrl || ctx.meta;
    let typing = b.search.lock().is_ok_and(|d| {
        matches!(
            d.visual,
            blinc_layout::stateful::TextFieldState::Focused
                | blinc_layout::stateful::TextFieldState::FocusedHovered
        )
    });
    let select = |to: usize| b.selected.signal().set(to);
    let last = || shown(&b).len().saturating_sub(1);
    let at = b.selected.get();
    let columns = COLUMNS.load(Ordering::Relaxed);
    match ctx.key_code {
        KEY_ESCAPE => set_open(false),
        KEY_1 if chord => {
            b.tab.signal().set(Tab::Installed);
            reset_paging();
        }
        KEY_2 if chord => {
            b.tab.signal().set(Tab::Gallery);
            reset_paging();
            app.send(Command::ThemeRefresh);
        }
        KEY_O if chord => pick_theme_file(app),
        KEY_L if chord && ctx.shift => set_scheme(app, Scheme::Light),
        KEY_D if chord && ctx.shift => set_scheme(app, Scheme::Dark),
        KEY_S if chord && ctx.shift => set_scheme(app, Scheme::System),
        KEY_LEFT if !typing => select(at.saturating_sub(1)),
        KEY_RIGHT if !typing => select((at + 1).min(last())),
        KEY_UP => select(at.saturating_sub(columns)),
        KEY_DOWN => select((at + columns).min(last())),
        KEY_ENTER => match shown(&b).into_iter().nth(at).map(|(p, _)| p) {
            Some(Primary::Apply(id)) => apply(app, id),
            Some(Primary::Install(id)) => app.send(Command::ThemeInstall(id)),
            _ => {}
        },
        KEY_DELETE if !typing => {
            if let Some((_, Some(id))) = shown(&b).into_iter().nth(at) {
                app.send(Command::ThemeRemove(id));
            }
        }
        _ => return false,
    }
    true
}

fn notice_strip(app: &App, b: &ThemeBindings, pad_x: f32) -> impl ElementBuilder {
    let notice = b.notice.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-themes-notice")
        .deps([notice.signal_id()])
        .on_state(move |_| {
            let Some(n) = notice.get() else {
                return div();
            };
            let dismiss = {
                let notice = notice.clone();
                button(tr(&app, "Dismiss", "关闭"))
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Small)
                    .on_click(move |_| notice.signal().set(None))
            };
            let mut strip = div().class("mq-themes-notice");
            if n.error {
                strip = strip.class("mq-themes-notice--error");
            }
            strip = strip
                .stack_layer()
                .absolute()
                .bottom(16.0)
                .left(pad_x)
                .right(pad_x)
                .flex_row()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w(0.0)
                        .child(ui_text(n.message.clone()).size(13.0)),
                );
            if let Some(retry) = n.retry.clone() {
                let notice = notice.clone();
                let app2 = app.clone();
                strip = strip.child(
                    button(tr(&app, "Retry", "重试"))
                        .variant(ButtonVariant::Outline)
                        .size(ButtonSize::Small)
                        .on_click(move |_| {
                            notice.signal().set(None);
                            app2.send(retry.clone());
                        }),
                );
            }
            strip.child(dismiss)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grid_fits_whole_cards() {
        let (cols, w) = layout(1200.0, PAD_X);
        assert!(cols >= 4);
        let used = cols as f32 * w + (cols as f32 - 1.0) * GAP;
        assert!((used - (1200.0 - 2.0 * PAD_X)).abs() < 0.5);
        assert!(w >= MIN_CARD);
        assert_eq!(layout(300.0, PAD_X).0, 1);
        let (embedded_cols, embedded_w) = layout(720.0, 0.0);
        let embedded_used =
            embedded_cols as f32 * embedded_w + (embedded_cols as f32 - 1.0) * GAP;
        assert!((embedded_used - 720.0).abs() < 0.5);
    }

    #[test]
    fn search_matches_name_tag_id_and_author() {
        let tags = vec!["neon".to_string(), "anime".to_string()];
        assert!(matches("", "X", "x", None, &[]));
        assert!(matches(
            "akiba",
            "Akiba Overdrive",
            "akiba-overdrive",
            None,
            &tags
        ));
        assert!(matches("neon", "Y", "y", None, &tags));
        assert!(matches(
            "collection",
            "Y",
            "y",
            Some("Muqun Theme Collection"),
            &[]
        ));
        assert!(!matches("jade", "Y", "y", None, &tags));
    }

    #[test]
    fn sizes_read_like_the_gallery() {
        assert_eq!(size_text(2_579_111), "2.5 MB");
        assert_eq!(size_text(1500), "2 KB");
    }
}
