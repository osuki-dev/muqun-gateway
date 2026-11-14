//! The file viewer: an overlay over the window that shows one file.
//!
//! **Entry point:** [`UiState::open_file`] with a path (`PathBuf`) or a
//! [`FileRef::Asset`] (a file the gateway presented: `files.present`, a
//! timeline image or file link, with the session folder it is relative to).
//! Text and code are shown highlighted with line numbers, Markdown rendered
//! (with its source a click away), images fit to the window or zoomed. Video,
//! audio, PDF and anything else get their metadata and "Open with system
//! app". Every file offers Save as… and Reveal in folder. Escape, the close
//! button or a click outside closes it.
//!
//! The file is read off the UI thread (`model/viewer.rs`); a file that is not
//! on this computer says so instead of failing quietly.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use rune::{col, el, rich, row, text, El, SurfaceFit};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, Kit, Markdown, SegmentedControl};

use super::copy::{tr, tr_ref};
use super::UiState;
use crate::config::Language;
use crate::model::viewer::{self as files, FileKind, FileRef, Loaded, Meta};

/// What the viewer shows.
#[derive(Debug, Clone)]
pub struct Viewer {
    /// The local file, when the target is on this computer.
    pub path: Option<PathBuf>,
    pub name: String,
    pub kind: FileKind,
    pub meta: Meta,
    pub body: Body,
    /// Images: `None` fits the window, else the scale (1.0 = actual pixels).
    pub zoom: Option<f32>,
    /// Markdown: show the source instead of the rendering.
    pub source: bool,
}

#[derive(Debug, Clone)]
pub enum Body {
    Loading,
    Text {
        text: Arc<str>,
        /// "1\n2\n…", one per line of `text`, built once.
        gutter: Arc<str>,
        lines: usize,
        truncated: bool,
    },
    Image,
    /// Not drawn here: metadata and the system app.
    External,
    /// The target is not a file on this computer.
    Missing,
    Failed(String),
}

// App glyphs (`assets/glyphs`), strokes copied onto each element.
static FILE: rune::Icon = rune::Icon {
    name: "viewer-file",
    view: 24.0,
    body: r#"<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Zm0 0v5h5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static SOURCE_FILE: rune::Icon = rune::Icon {
    name: "viewer-source-file",
    view: 24.0,
    body: r#"<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Zm0 0v5h5m-9 5-2 2 2 2m4-4 2 2-2 2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static DOCUMENT: rune::Icon = rune::Icon {
    name: "viewer-document",
    view: 24.0,
    body: r#"<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Zm0 0v5h5M9 13h6m-6 4h4" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static PHOTO_FILE: rune::Icon = rune::Icon {
    name: "viewer-photo-file",
    view: 24.0,
    body: r#"<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Zm0 0v5h5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="m8 18 3-4 3 3 2-2m-7-4h.01" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static VIDEO: rune::Icon = rune::Icon {
    name: "viewer-video",
    view: 24.0,
    body: r#"<rect x="3" y="4" width="18" height="16" rx="3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="m10 8 6 4-6 4V8ZM3 8h3m-3 8h3m12-8h3m-3 8h3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static AUDIO: rune::Icon = rune::Icon {
    name: "viewer-audio",
    view: 24.0,
    body: r#"<path d="M4 12V9a8 8 0 0 1 16 0v3M4 12H3v7h4v-7H4Zm16 0h1v7h-4v-7h3ZM17 19a4 4 0 0 1-4 3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static OPEN_EXTERNAL: rune::Icon = rune::Icon {
    name: "viewer-open-external",
    view: 24.0,
    body: r#"<path d="M13 3h8v8m0-8L10 14M8 5H5a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static OPEN_FOLDER: rune::Icon = rune::Icon {
    name: "viewer-open-folder",
    view: 24.0,
    body: r#"<path d="M3 9V6a2 2 0 0 1 2-2h5l2 3h6a2 2 0 0 1 2 2M5 20a2 2 0 0 1-2-2v-6h19l-3 8H5Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};
static DOWNLOAD: rune::Icon = rune::Icon {
    name: "viewer-download",
    view: 24.0,
    body: r#"<path d="M12 3v12m-5-5 5 5 5-5M4 16v3a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#,
};

const ZOOM_STEPS: [f32; 9] = [0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0, 8.0];

impl UiState {
    /// Show `target` in the viewer overlay, replacing whatever it showed.
    pub fn open_file(&mut self, target: impl Into<FileRef>) {
        let target = target.into();
        let path = target.resolve();
        let name = path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.display_name());
        let kind = files::kind_of(path.as_deref().unwrap_or(Path::new(&name)), target.mime());
        if self.cfg.general.files_in_editor && kind.reads_text() {
            if let Some(path) = path.as_deref() {
                self.open_editor_file(path);
                return;
            }
        }
        let mut meta = path.as_deref().map(Meta::of).unwrap_or_default();
        if let FileRef::Asset { asset, .. } = &target {
            meta.size = meta.size.or(asset.size);
        }
        let body = match &path {
            None => Body::Missing,
            Some(path) if kind.reads_text() || kind == FileKind::Other => {
                // An unknown file is read too: text comes back as text.
                files::spawn_load(path.clone());
                Body::Loading
            }
            Some(_) if kind == FileKind::Image => Body::Image,
            Some(_) => Body::External,
        };
        self.viewer = Some(Viewer {
            path,
            name,
            kind,
            meta,
            body,
            zoom: None,
            source: false,
        });
        self.show_file_window();
    }

    pub fn show_file_window(&mut self) {
        let newly_opened = self.tile_of_plugin("files").is_none();
        self.open_plugin_tile("files");
        if let Some(i) = self.tile_of_plugin("files") {
            if let (Some(v), super::TileContent::Custom { title, .. }) =
                (&self.viewer, &mut self.tiles[i].content)
            {
                *title = v.name.clone();
            }
            if newly_opened {
                let [width, height] = self.tile_grid;
                let w = if width > 100.0 { width } else { 1120.0 };
                let h = if height > 100.0 { height } else { 760.0 };
                self.tiles[i].floating_rect = Some([w * 0.08, h * 0.06, w * 0.84, h * 0.88]);
            }
            if !self.tiles[i].popped {
                self.toggle_tile_pop_at(i);
            }
        }
    }

    pub fn file_window_focused(&self) -> bool {
        if self.viewer.is_none() || self.term.sink.focused {
            return false;
        }
        if let Some(id) = &self.active_plugin_page {
            return id == "files";
        }
        self.active_plugin_drawer.as_deref() == Some("files")
            || self.tile_of_plugin("files") == Some(self.focus)
    }

    pub fn close_viewer(&mut self) {
        self.viewer = None;
        if self.active_plugin_page.as_deref() == Some("files") {
            self.active_plugin_page = None;
            self.plugin_page_origin = None;
        }
        if self.active_plugin_drawer.as_deref() == Some("files") {
            self.close_plugin_drawer();
        }
        if let Some(i) = self.tile_of_plugin("files") {
            self.close_tile(i);
        }
    }

    /// Open a file with the configured editor. Terminal editors (such as nvim)
    /// open directly in the embedded terminal without slide animations.
    pub fn open_editor_file(&mut self, path: &Path) {
        let custom = self.cfg.general.editor_command.clone();
        let choice = Some(self.cfg.general.editor);
        let language = self.cfg.general.language;

        if let Some(editor) =
            crate::platform::editors::resolve_terminal_editor(custom.as_deref(), choice)
        {
            self.open_file_in_terminal(path, &editor);
        } else if let Some(cmd) = custom.as_deref().filter(|c| !c.trim().is_empty()) {
            match crate::platform::editors::try_custom_command(cmd, path) {
                Ok(true) => {}
                _ => {
                    let title = crate::ui::copy::tr(language, "Could not open editor");
                    let detail = crate::ui::copy::tr(
                        language,
                        "Command failed to launch. Check Settings > General.",
                    );
                    self.toast(title, Some(format!("{detail} ({cmd})")));
                }
            }
        } else if let Some(c) = choice {
            match c {
                crate::platform::editors::Editor::FileManager => {
                    crate::platform::shell::reveal(path);
                }
                gui_editor => match crate::platform::editors::try_editor_choice(gui_editor, path) {
                    Ok(true) => {}
                    _ => {
                        if let Ok(true) = crate::platform::editors::try_open_editor(path) {
                            return;
                        }
                        if let Some(term_ed) =
                            crate::platform::editors::detect_default_terminal_editor()
                        {
                            self.open_file_in_terminal(path, &term_ed);
                            return;
                        }
                        let name = gui_editor.product().unwrap_or("Editor");
                        let title = crate::ui::copy::tr(language, "Could not open editor");
                        let detail = crate::ui::copy::tr(
                            language,
                            "Application not found. Check Settings > General.",
                        );
                        self.toast(title, Some(format!("{name}: {detail}")));
                    }
                },
            }
        }
    }

    /// The reader answered for `path` (ignored when the viewer moved on).
    pub fn viewer_loaded(&mut self, path: &Path, loaded: Loaded) {
        let Some(v) = self
            .viewer
            .as_mut()
            .filter(|v| v.path.as_deref() == Some(path))
        else {
            return;
        };
        v.body = match loaded {
            Loaded::Text { text, truncated } => {
                if v.kind == FileKind::Other {
                    v.kind = FileKind::Text(None);
                }
                text_body(text, truncated)
            }
            Loaded::Binary => Body::External,
            Loaded::Failed(err) => Body::Failed(err),
        };
    }

    /// Save as… finished.
    pub fn file_saved(&mut self, result: Result<PathBuf, String>) {
        let language = self.cfg.general.language;
        match result {
            Ok(path) => self.toast(tr(language, "Saved"), Some(path.display().to_string())),
            Err(err) => self.toast(tr(language, "Could not save"), Some(err)),
        }
    }
}

/// A text body: the gutter is built once here, not on every frame.
pub fn text_body(text: Arc<str>, truncated: bool) -> Body {
    let text: Arc<str> = match text.strip_suffix('\n') {
        Some(body) => Arc::from(body),
        None => text,
    };
    let lines = text.split('\n').count().max(1);
    let mut gutter = String::with_capacity(lines * 5);
    for n in 1..=lines {
        if n > 1 {
            gutter.push('\n');
        }
        gutter.push_str(&n.to_string());
    }
    Body::Text {
        text,
        gutter: Arc::from(gutter),
        lines,
        truncated,
    }
}

/// File content uses the same host-owned window as any extension.
pub fn content(s: &UiState, k: &Kit, scale: f32) -> El {
    let Some(v) = s.viewer.as_ref() else {
        return el().center().child(text(tr_ref(
            s.cfg.general.language,
            "Open a file to preview it",
        )));
    };
    col()
        .key("viewer-panel")
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(header(v, k, s.cfg.general.language))
        .child(body(s, v, k, scale, s.cfg.general.language))
}

fn kind_label(kind: FileKind, language: Language) -> String {
    match kind {
        FileKind::Text(Some(lang)) => lang_label(lang).to_string(),
        FileKind::Text(None) => tr(language, "Text"),
        FileKind::Markdown => "Markdown".into(),
        FileKind::Image => tr(language, "Image"),
        FileKind::Video => tr(language, "Video"),
        FileKind::Audio => tr(language, "Audio"),
        FileKind::Pdf => "PDF".into(),
        FileKind::Other => tr(language, "File"),
    }
}

fn lang_label(lang: &str) -> &str {
    match lang {
        "rust" => "Rust",
        "javascript" => "JavaScript",
        "jsx" => "JSX",
        "typescript" => "TypeScript",
        "tsx" => "TSX",
        "json" => "JSON",
        "python" => "Python",
        "go" => "Go",
        "swift" => "Swift",
        "shell" => "Shell",
        "sql" => "SQL",
        "html" => "HTML",
        "xml" => "XML",
        "css" => "CSS",
        "yaml" => "YAML",
        "diff" => "Diff",
        other => other,
    }
}

fn kind_glyph(kind: FileKind) -> &'static rune::Icon {
    match kind {
        FileKind::Text(Some(_)) => &SOURCE_FILE,
        FileKind::Text(None) | FileKind::Markdown | FileKind::Pdf => &DOCUMENT,
        FileKind::Image => &PHOTO_FILE,
        FileKind::Video => &VIDEO,
        FileKind::Audio => &AUDIO,
        FileKind::Other => &FILE,
    }
}

/// "Rust · 14.2 KB · 1,204 lines · modified 3 min ago"
fn meta_line(v: &Viewer, language: Language) -> String {
    let mut parts = vec![kind_label(v.kind, language)];
    if let Some(size) = v.meta.size {
        parts.push(files::human_size(size));
    }
    if let Body::Text { lines, .. } = &v.body {
        parts.push(tr(language, &format!("{lines} lines")));
    }
    if let Some(when) = v.meta.modified {
        parts.push(modified_label(when, SystemTime::now(), language));
    }
    parts.join(" · ")
}

fn modified_label(when: SystemTime, now: SystemTime, language: Language) -> String {
    let secs = now.duration_since(when).map_or(0, |d| d.as_secs());
    let en = match secs {
        0..=59 => tr(language, "modified just now"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=604_799 => format!("{}d ago", secs / 86_400),
        _ => {
            let date = civil_date(when);
            format!("modified {date}")
        }
    };
    tr(language, &en)
}

/// `YYYY-MM-DD` (UTC) of `when`.
fn civil_date(when: SystemTime) -> String {
    let days = when
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    // Howard Hinnant's days-from-civil, inverted.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn header(v: &Viewer, k: &Kit, language: Language) -> El {
    let folder = v.path.as_deref().and_then(Path::parent).map(home_relative);
    let mut title = col()
        .key("viewer-title")
        .grow(1.0)
        .shrink()
        .gap(2.0)
        .child(
            text(v.name.clone())
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate_middle()
                .help_when_truncated(v.name.clone()),
        )
        .child(
            text(meta_line(v, language))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .truncate(),
        );
    if let Some(folder) = folder {
        title = title.child(
            text(folder.clone())
                .font(k.ty.small)
                .color(k.c.text_faint)
                .truncate_head()
                .help_when_truncated(folder)
                .selectable(true),
        );
    }
    let mut bar = row()
        .key("viewer-header")
        .flex_wrap()
        .w_full()
        .gap(12.0)
        .center_y()
        .px(18.0)
        .py(14.0)
        .child(
            el().key("viewer-kind")
                .size(36.0, 36.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_02)
                .center()
                .child(rune::icon(kind_glyph(v.kind), 18.0).color(k.c.text_muted)),
        )
        .child(title);

    if v.kind == FileKind::Markdown && matches!(v.body, Body::Text { .. }) {
        let labels = [tr_ref(language, "Rendered"), tr_ref(language, "Source")];
        bar = bar.child(
            SegmentedControl::new(&labels, Some(usize::from(v.source)))
                .fit()
                .label(tr_ref(language, "Show"))
                .on_pick(|s: &mut UiState, i, _: &mut rune::Tx| {
                    if let Some(v) = s.viewer.as_mut() {
                        v.source = i == 1;
                    }
                })
                .build(k)
                .key("viewer-md-mode"),
        );
    }
    if matches!(v.body, Body::Image) {
        bar = bar.child(zoom_controls(v, k, language));
    }
    if let Some(path) = v.path.clone() {
        let open = path.clone();
        let reveal = path.clone();
        let is_text = v.kind.reads_text();
        bar = bar
            .child(
                Button::new(if is_text {
                    tr_ref(language, "Open in editor")
                } else {
                    tr_ref(language, "Open with system app")
                })
                .icon(&OPEN_EXTERNAL)
                .variant(ButtonVariant::Ghost)
                .on_press(move |s: &mut UiState| {
                    s.open_editor_file(&open);
                })
                .build(k)
                .key("viewer-open"),
            )
            .child(
                Button::new(tr_ref(language, "Reveal in folder"))
                    .icon(&OPEN_FOLDER)
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |_: &mut UiState| crate::platform::shell::reveal(&reveal))
                    .build(k)
                    .key("viewer-reveal"),
            )
            .child(
                Button::new(tr_ref(language, "Save as…"))
                    .icon(&DOWNLOAD)
                    .variant(ButtonVariant::Outline)
                    .on_press(move |_: &mut UiState| {
                        crate::platform::dialogs::save_copy(path.clone())
                    })
                    .build(k)
                    .key("viewer-save"),
            );
    }
    bar
}

fn zoom_controls(v: &Viewer, k: &Kit, language: Language) -> El {
    let step = |dir: i32| {
        move |s: &mut UiState| {
            if let Some(v) = s.viewer.as_mut() {
                let now = v.zoom.unwrap_or(1.0);
                let next = if dir > 0 {
                    ZOOM_STEPS.iter().copied().find(|z| *z > now + 0.01)
                } else {
                    ZOOM_STEPS.iter().rev().copied().find(|z| *z < now - 0.01)
                };
                v.zoom = Some(next.unwrap_or(now));
            }
        }
    };
    let fit = v.zoom.is_none();
    let label = match v.zoom {
        None => tr(language, "Fit"),
        Some(z) => format!("{:.0}%", z * 100.0),
    };
    row()
        .key("viewer-zoom")
        .gap(4.0)
        .center_y()
        .child(
            Button::new("−")
                .variant(ButtonVariant::Ghost)
                .on_press(step(-1))
                .build(k)
                .key("zoom-out"),
        )
        .child(
            Button::new(label)
                .variant(ButtonVariant::Ghost)
                .help(tr(language, "Fit to the window / actual size"))
                .on_press(move |s: &mut UiState| {
                    if let Some(v) = s.viewer.as_mut() {
                        v.zoom = if fit { Some(1.0) } else { None };
                    }
                })
                .build(k)
                .key("zoom-fit"),
        )
        .child(
            Button::new("+")
                .variant(ButtonVariant::Ghost)
                .on_press(step(1))
                .build(k)
                .key("zoom-in"),
        )
}

fn home_relative(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn body(s: &UiState, v: &Viewer, k: &Kit, scale: f32, language: Language) -> El {
    let frame = col().key("viewer-body").w_full().grow(1.0).min_h(0.0);
    match &v.body {
        Body::Loading => frame.center().child(
            text(tr(language, "Opening…"))
                .font(k.ty.ui)
                .color(k.c.text_muted),
        ),
        Body::Text {
            text: source,
            gutter,
            lines,
            truncated,
        } => {
            let content = if v.kind == FileKind::Markdown && !v.source {
                markdown(k, source)
            } else {
                let lang = match v.kind {
                    FileKind::Text(lang) => lang,
                    _ => Some("markdown"),
                };
                code(k, source, gutter, *lines, lang)
            };
            let mut frame = frame.child(content);
            if *truncated {
                frame = frame.child(truncation_note(k, language));
            }
            frame
        }
        Body::Image => frame.child(image(s, v, k, scale, language)),
        Body::External => frame.center().child(external(v, k, language)),
        Body::Missing => frame.center().child(notice(
            k,
            tr(language, "Not on this computer"),
            tr(
                language,
                "The file is on another computer or no longer exists. Open it from the App paired with that computer."
            ),
        )),
        Body::Failed(err) => frame.center().child(notice(
            k,
            tr(language, "Could not read the file"),
            err.clone(),
        )),
    }
}

fn code(k: &Kit, source: &Arc<str>, gutter: &Arc<str>, lines: usize, lang: Option<&str>) -> El {
    let lang = lang.filter(|l| syntax::known(l));
    let spans = match lang {
        Some(lang) => SyntaxTheme::from_kit(k)
            .spans(&syntax::highlight(Some(lang), source), 0)
            .into_iter()
            .map(|span| span.mono())
            .collect(),
        None => Vec::new(),
    };
    row()
        .key("viewer-code")
        .items_start()
        .child(
            text(gutter.to_string())
                .key("viewer-gutter")
                .font(k.ty.mono)
                .color(k.c.text_faint)
                .wrap()
                .align(rune::Align::End)
                // Room for the widest number; aligned right, as editors do.
                .w(digits(lines) as f32 * k.ty.mono.size * 0.62 + 8.0)
                .ml(10.0)
                .mr(16.0)
                .py(14.0),
        )
        .child(
            rich(source.to_string(), spans)
                .key("viewer-text")
                .font(k.ty.mono)
                .color(k.c.text_base)
                .selectable(true)
                .selection_color(k.selection())
                .pr(24.0)
                .py(14.0),
        )
        .wrap_in_scroll()
}

fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

trait ScrollBoth {
    fn wrap_in_scroll(self) -> El;
}

impl ScrollBoth for El {
    /// Both ways: code keeps its lines, long ones scroll sideways.
    fn wrap_in_scroll(self) -> El {
        col()
            .key("viewer-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(self)
            .scroll_xy()
    }
}

fn markdown(k: &Kit, source: &Arc<str>) -> El {
    col()
        .key("viewer-md")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(
            col().key("viewer-md-col").w_full().items_center().child(
                col()
                    .key("viewer-md-page")
                    .w_full()
                    .max_w(760.0)
                    .px(32.0)
                    .py(24.0)
                    .child(
                        Markdown::new(source.to_string())
                            .selectable(true)
                            .on_link_tx(|s: &mut UiState, url: &str, tx: &mut rune::Tx| {
                                crate::ui::parts::open_markdown_link(s, url, tx);
                            })
                            .build(k),
                    ),
            ),
        )
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
}

fn truncation_note(k: &Kit, language: Language) -> El {
    row()
        .key("viewer-truncated")
        .w_full()
        .px(18.0)
        .py(8.0)
        .fill(k.c.layer_02)
        .child(
            text(tr(
                language,
                &format!(
                    "Showing the first {}. Open with system app to see the whole file.",
                    files::human_size(files::MAX_TEXT_BYTES)
                ),
            ))
            .font(k.ty.small)
            .color(k.c.text_muted),
        )
}

fn image(s: &UiState, v: &Viewer, k: &Kit, scale: f32, language: Language) -> El {
    let (Some(images), Some(path)) = (s.images.as_ref(), v.path.as_ref()) else {
        return el().key("viewer-image").w_full().grow(1.0);
    };
    let picture = images.file(path, 1024.0, scale.max(1.0));
    if picture.error().is_some() {
        return col()
            .key("viewer-image")
            .w_full()
            .grow(1.0)
            .center()
            .child(external(v, k, language));
    }
    // A checkerboard-free, theme-coloured stage so transparent pictures read.
    let stage = col()
        .key("viewer-image")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(k.c.bg_deep);
    match (v.zoom, picture.size()) {
        (Some(zoom), Some([w, h])) => {
            let (w, h) = (w as f32 / scale * zoom, h as f32 / scale * zoom);
            stage
                .child(
                    col().key("viewer-image-canvas").padding(24.0).child(
                        el().key("viewer-picture")
                            .pixels_fit(picture.pixels(), SurfaceFit::Fit)
                            .size(w, h),
                    ),
                )
                .scroll_xy()
        }
        _ => stage.padding(24.0).child(
            el().key("viewer-picture")
                .pixels_fit(picture.pixels(), SurfaceFit::Fit)
                .w_full()
                .grow(1.0),
        ),
    }
}

fn external(v: &Viewer, k: &Kit, language: Language) -> El {
    let what = match v.kind {
        FileKind::Video => tr(language, "Videos play in your system player."),
        FileKind::Audio => tr(language, "Audio plays in your system player."),
        FileKind::Pdf => tr(language, "PDFs open in your system viewer."),
        _ => tr(language, "Muqun can't show this kind of file."),
    };
    let mut card = col()
        .key("viewer-external")
        .gap(10.0)
        .items_center()
        .max_w(420.0)
        .child(
            el().key("viewer-external-mark")
                .size(64.0, 64.0)
                .radius(k.radius.xl)
                .fill(k.c.layer_02)
                .center()
                .child(rune::icon(kind_glyph(v.kind), 30.0).color(k.c.text_muted)),
        )
        .child(
            text(v.name.clone())
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate_middle(),
        )
        .child(
            text(meta_line(v, language))
                .font(k.ty.small)
                .color(k.c.text_muted),
        )
        .child(text(what).font(k.ty.ui).color(k.c.text_muted));
    if let Some(path) = v.path.clone() {
        let is_text = v.kind.reads_text();
        card = card.child(
            Button::new(if is_text {
                tr_ref(language, "Open in editor")
            } else {
                tr_ref(language, "Open with system app")
            })
            .icon(&OPEN_EXTERNAL)
            .variant(ButtonVariant::Contrast)
            .on_press(move |s: &mut UiState| {
                s.open_editor_file(&path);
            })
            .build(k)
            .key("viewer-external-open")
            .mt(6.0),
        );
    }
    card
}

fn notice(k: &Kit, title: String, detail: String) -> El {
    col()
        .key("viewer-notice")
        .gap(6.0)
        .items_center()
        .max_w(440.0)
        .child(text(title).font(k.ty.ui_medium).color(k.c.text_base))
        .child(
            text(detail)
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap()
                .align(rune::Align::Center),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_neovim_opens_directly_without_a_preview_window() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.cfg.general.editor = crate::platform::editors::Editor::Neovim;
        s.cfg.general.files_in_editor = true;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/config.rs");
        s.open_file(path.clone());
        assert!(s.viewer.is_none());
        assert!(s.tile_of_plugin("files").is_none());
        assert!(s.term.open && s.term.sink.focused);
        assert_eq!(
            s.term.pending_command.as_deref(),
            Some(format!("nvim '{}'\n", path.display()).as_str())
        );
    }

    #[test]
    fn the_gutter_numbers_every_line_once() {
        let Body::Text { gutter, lines, .. } = text_body(Arc::from("a\nb\n\nc\n"), false) else {
            panic!("text");
        };
        assert_eq!(lines, 4);
        assert_eq!(&*gutter, "1\n2\n3\n4");
    }

    #[test]
    fn old_files_show_their_date() {
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_794_096_000);
        assert_eq!(civil_date(when), "2026-11-08");
    }
}
