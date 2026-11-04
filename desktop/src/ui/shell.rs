//! The window's frame (architecture §6.0‴ A): three floating top capsules
//! (brand = sidebar width, centred title+actions, optional right), then the
//! left sidebar (a resizable 260 px column, or a 56 px rail under 960 px /
//! when folded with Ctrl/Cmd+B), the flexible main panel, and an optional
//! resizable right column with Queue / Git tabs (and room for more later).
//!
//! The main panel shows exactly one of: Settings, the Inbox, Home (the
//! start page, nothing selected), or the selected session.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::composer::{self, chips};
use super::glyphs::Glyph;
use super::icons;
use super::sessions::{self, title_of};
use super::text::{display_text, mono_text, ui_text};
use super::{errors, home, settings, timeline, App};
use crate::branding::{mark_data_uri, APP_NAME};
use crate::config::{ConfigEdit, UiPrefs};
use crate::model::commands::Command;
use crate::model::view::RightTab;
use crate::platform::editors::{self, Editor};
use blinc_app::windowed::WindowedContext;
use blinc_cn::prelude::{button, dropdown_menu_custom, ButtonSize, ButtonVariant};
use blinc_core::State;
use blinc_layout::prelude::*;

pub const TOP_BAR_HEIGHT: f32 = 48.0;
/// Matches `--mq-space-3`: body inset under the pill and beside the columns,
/// so the sidebar's bottom radius reads against the window background.
pub const FRAME_PAD: f32 = 12.0;
/// Under this window width the sidebar folds to its rail.
pub const RAIL_BELOW: f32 = 960.0;
/// A second press on the sidebar's edge within this resets its width.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// The editor "Open in …" uses: the last one chosen, remembered.
pub fn editor_state(app: &App) -> State<Editor> {
    let editor = app.config.get().general.editor;
    blinc_core::BlincContextState::get().use_state_keyed("mq.editor", move || editor)
}

/// Open `folder` with `editor`, and make it the one "Open in" offers first.
pub fn open_in(app: &App, editor: Editor, folder: &std::path::Path) {
    let state = editor_state(app);
    if state.get() != editor {
        state.signal().set(editor);
        app.send(Command::Edit(ConfigEdit::Editor(editor)));
    }
    if editors::open(editor, folder).is_err() {
        app.bindings
            .error
            .signal()
            .set(Some(crate::model::inbox::UiError {
                message: format!("{}: {}", app.strings.open_in(editor), folder.display()),
                detail: None,
                unreachable: false,
            }));
    }
}

/// The open session's project folder: its checkout root when resolved, else
/// its own folder.
fn session_root(app: &App) -> Option<std::path::PathBuf> {
    let info = app.bindings.header.get()?;
    let dir = info.directory.clone().filter(|d| !d.is_empty())?;
    let dirs = app.bindings.dirs.get();
    Some(
        dirs.get(dir.trim_end_matches(['/', '\\']))
            .map(|i| i.root.clone())
            .unwrap_or_else(|| dir.into()),
    )
}

/// A 28 px ghost button holding one App glyph: no border, a surface on hover.
fn ghost_icon(glyph: Glyph) -> Div {
    div()
        .class("mq-icon-button")
        .w(28.0)
        .h(28.0)
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .child(icons::tinted(glyph, 16.0))
}

/// Three floating capsules: brand (sidebar width) | title+actions (content
/// width, centred) | optional right (FPS for now; omitted when empty).
pub fn top_bar(app: &App, window_width: f32) -> Div {
    let collapsed = sidebar_collapsed();
    let width = sidebar_width(app);
    let app = app.clone();
    let bar = stateful_with_key::<NoState>("mq-topbar")
        .deps([collapsed.signal_id(), width.signal_id()])
        .on_state(move |_| {
            let rail = collapsed.get() || window_width < RAIL_BELOW;
            let side = if rail {
                sessions::RAIL_WIDTH
            } else {
                width.get()
            };
            let home_app = app.clone();
            let mut row = div()
                .class("mq-topbar")
                .w_full()
                .h(TOP_BAR_HEIGHT)
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .child(
                    div()
                        .class("mq-topbar-pill")
                        .class("mq-topbar-left")
                        .w(side)
                        .h(36.0)
                        .flex_row()
                        .items_center()
                        .flex_shrink_0()
                        .overflow_clip()
                        .child(
                            div()
                                .class("mq-brand")
                                .flex_row()
                                .items_center()
                                .flex_shrink_0()
                                .cursor_pointer()
                                .child(image(mark_data_uri()).size(22.0, 22.0).rounded(6.0))
                                .child(
                                    ui_text(APP_NAME)
                                        .size(14.0)
                                        .semibold()
                                        .no_wrap()
                                        .no_cursor(),
                                )
                                .on_click(move |_| home_app.drain.go_home()),
                        ),
                )
                .child(
                    div()
                        .class("mq-topbar-center-slot")
                        .flex_1()
                        .min_w(0.0)
                        .h(36.0)
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .class("mq-topbar-pill")
                                .class("mq-topbar-center")
                                .h(36.0)
                                .flex_row()
                                .items_center()
                                .flex_shrink_0()
                                .overflow_clip()
                                .child(title(&app))
                                .child(
                                    div()
                                        .class("mq-topbar-actions")
                                        .flex_row()
                                        .items_center()
                                        .flex_shrink_0()
                                        .child(open_in_button(&app))
                                        .child(git_toggle(&app)),
                                ),
                        ),
                );
            // Right capsule only when it has something to show.
            if let Some(right) = topbar_right() {
                row = row.child(right);
            }
            row
        });
    div().w_full().h(TOP_BAR_HEIGHT).flex_shrink_0().child(bar)
}

/// Right-zone capsule. `None` hides the zone entirely (no reserved width).
fn topbar_right() -> Option<Div> {
    super::perf::pill()
}

/// The open session's title (15 px display serif); empty on the start page
/// (the brand already names the app in the pill).
fn title(app: &App) -> impl ElementBuilder {
    let header = app.bindings.header.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-topbar-title")
        .deps([header.signal_id()])
        .on_state(move |_| {
            let line = div()
                .class("mq-topbar-title")
                .flex_row()
                .items_center()
                .justify_center()
                .min_w(0.0)
                .overflow_clip();
            let Some(info) = header.get() else {
                return line;
            };
            line.child(
                div()
                    .class("mq-topbar-session")
                    .min_w(0.0)
                    .overflow_clip()
                    .child(
                        display_text(title_of(&app, &info))
                            .size(15.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
            )
        })
}

/// Git control in the top pill: shows `+n −m` while a session has changes,
/// otherwise the branch icon. Always toggles the right column.
fn git_toggle(app: &App) -> impl ElementBuilder {
    let diff = app.bindings.diff.clone();
    let header = app.bindings.header.clone();
    let panel = app.bindings.diff_panel.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-git-toggle")
        .deps([diff.signal_id(), header.signal_id(), panel.signal_id()])
        .on_state(move |_| {
            let open = header.get().map(|h| h.asid.0.clone());
            let stat = diff
                .get()
                .filter(|d| Some(&d.asid) == open.as_ref() && d.files > 0);
            let click = app.clone();
            let mut row = if let Some(stat) = stat {
                div()
                    .class("mq-ghost")
                    .class("mq-diff-count")
                    .h(28.0)
                    .flex_row()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div().class("mq-diff-added").child(
                            mono_text(format!("+{}", stat.added))
                                .size(12.0)
                                .no_wrap()
                                .no_cursor(),
                        ),
                    )
                    .child(
                        div().class("mq-diff-removed").child(
                            mono_text(format!("−{}", stat.removed))
                                .size(12.0)
                                .no_wrap()
                                .no_cursor(),
                        ),
                    )
            } else {
                ghost_icon(Glyph::Branch)
            };
            if panel.get() {
                row = row.class("mq-diff-count--open");
            }
            row.on_click(move |_| toggle_right(&click, RightTab::Git))
        })
}

/// `Open in <editor> ▾`: the primary opens the session's project in the
/// last editor used; the chevron offers the others. Only while a session
/// with a folder is open.
fn open_in_button(app: &App) -> impl ElementBuilder {
    let header = app.bindings.header.clone();
    let editor = editor_state(app);
    let app = app.clone();
    stateful_with_key::<NoState>("mq-open-in")
        .deps([header.signal_id(), editor.signal_id()])
        .on_state(move |_| {
            let Some(root) = session_root(&app) else {
                return div();
            };
            let current = editor.get();
            let main_app = app.clone();
            let main_root = root.clone();
            let mut menu = dropdown_menu_custom(|_open| {
                div()
                    .class("mq-ghost")
                    .class("mq-ghost--chevron")
                    .h(28.0)
                    .flex_row()
                    .items_center()
                    .cursor_pointer()
                    .child(icons::tinted(Glyph::DisclosureDown, 14.0))
            })
            .min_width(200.0);
            for choice in Editor::ALL {
                let pick_app = app.clone();
                let pick_root = root.clone();
                menu = menu.item(app.strings.open_in(choice), move || {
                    open_in(&pick_app, choice, &pick_root)
                });
            }
            div()
                .class("mq-open-in")
                .flex_row()
                .items_center()
                .child(
                    div()
                        .class("mq-ghost")
                        .h(28.0)
                        .flex_row()
                        .items_center()
                        .cursor_pointer()
                        .child(icons::tinted(Glyph::Code, 14.0))
                        .child(
                            ui_text(app.strings.open_in(current))
                                .size(13.0)
                                .no_wrap()
                                .no_cursor(),
                        )
                        .on_click(move |_| open_in(&main_app, current, &main_root)),
                )
                .child(menu)
        })
}

/// Open a changed path with the preferred editor command, else the OS opener.
pub fn open_diff_file(app: &App, file: &str) {
    let path = std::path::Path::new(file);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match session_root(app) {
            Some(root) => root.join(file),
            None => path.to_path_buf(),
        }
    };
    let command = app.config.get().general.editor_command;
    let mut words = command.as_deref().unwrap_or("").split_whitespace();
    let Some(program) = words.next() else {
        crate::platform::shell::open_path(&path);
        return;
    };
    match std::process::Command::new(program)
        .args(words)
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => tracing::warn!(program, "could not open {}: {err}", path.display()),
    }
}

/// Preferred right width, clamped so left + main keep [`UiPrefs::MAIN_MIN`].
pub fn clamp_right_width(want: f32, window: f32, side: f32) -> f32 {
    let available = (window - 2.0 * FRAME_PAD - side - UiPrefs::MAIN_MIN - 8.0).max(0.0);
    let max = available.clamp(UiPrefs::RIGHT_MIN, UiPrefs::RIGHT_MAX);
    want.clamp(UiPrefs::RIGHT_MIN, max)
}

/// Width the right column occupies (0 while closed).
pub fn right_width(app: &App) -> f32 {
    if !app.bindings.diff_panel.get() {
        return 0.0;
    }
    let (vw, _) = blinc_core::BlincContextState::get().viewport_size();
    let side = side_width(app, vw);
    clamp_right_width(app.config.get().ui.right(), vw, side)
}

/// Open the right column on `tab` (or switch tab if already open).
pub fn open_right(app: &App, tab: RightTab) {
    app.bindings.right_tab.signal().set(tab);
    app.bindings.diff_panel.signal().set(true);
}

/// Toggle: same tab while open closes; otherwise opens onto `tab`.
pub fn toggle_right(app: &App, tab: RightTab) {
    if app.bindings.diff_panel.get() && app.bindings.right_tab.get() == tab {
        close_right(app);
    } else {
        open_right(app, tab);
    }
}

/// Close the right column (Esc / leaving a session).
pub fn close_right(app: &App) {
    if app.bindings.diff_panel.get() {
        app.bindings.diff_panel.signal().set(false);
        app.bindings.diff_focus.signal().set(None);
    }
}

/// Right-column stub kept for the root tree call site (content is in-column).
pub fn diff_overlay(app: &App) -> impl ElementBuilder {
    let _ = app;
    div()
}

fn right_panel_width(app: &App) -> State<f32> {
    let width = app.config.get().ui.right();
    blinc_core::BlincContextState::get().use_state_keyed("mq.right.width", move || width)
}

/// Drag handle on the left edge of the right column.
fn right_resize_handle(app: &App, width: State<f32>, window: f32, side: f32) -> Div {
    static RIGHT_DRAG: OnceLock<Mutex<(f32, f32, Option<Instant>)>> = OnceLock::new();
    let drag = RIGHT_DRAG.get_or_init(|| Mutex::new((0.0, 0.0, None)));
    let (down_w, move_w, end_w) = (width.clone(), width.clone(), width);
    let (reset_app, end_app) = (app.clone(), app.clone());
    div()
        .class("mq-side-handle")
        .class("mq-right-handle")
        .absolute()
        .top(0.0)
        .bottom(0.0)
        .left(-3.0)
        .w(6.0)
        .cursor(CursorStyle::ResizeEW)
        .on_mouse_down(move |e| {
            let mut d = drag.lock().unwrap();
            let now = Instant::now();
            if d.2.is_some_and(|last| now - last < DOUBLE_CLICK) {
                d.2 = None;
                let reset = clamp_right_width(UiPrefs::RIGHT_DEFAULT, window, side);
                down_w.set(reset);
                reset_app.send(Command::Edit(ConfigEdit::RightWidth(reset)));
                return;
            }
            *d = (e.mouse_x, down_w.get(), Some(now));
        })
        .on_drag(move |e| {
            let (x, start, _) = *drag.lock().unwrap();
            // Dragging the left edge right shrinks the panel.
            let next = clamp_right_width(start - (e.mouse_x - x), window, side);
            if (move_w.get() - next).abs() >= 0.5 {
                move_w.set(next);
            }
        })
        .on_drag_end(move |_| {
            end_app.send(Command::Edit(ConfigEdit::RightWidth(end_w.get())));
        })
}

fn right_tab_chip(app: &App, tab: RightTab, label: &str, badge: Option<usize>) -> Div {
    let active = app.bindings.right_tab.get() == tab;
    let click = app.clone();
    let mut chip = div()
        .class("mq-right-tab")
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(
            ui_text(label.to_string())
                .size(12.0)
                .semibold()
                .no_wrap()
                .no_cursor(),
        )
        .on_click(move |_| {
            click.bindings.right_tab.signal().set(tab);
        });
    if active {
        chip = chip.class("mq-right-tab--active");
    }
    if let Some(n) = badge.filter(|n| *n > 0) {
        chip = chip.child(
            div()
                .class("mq-right-tab-badge")
                .child(mono_text(format!("{n}")).size(10.0).no_wrap().no_cursor()),
        );
    }
    chip
}

fn right_column(app: &App, height: f32, width: State<f32>, window: f32, side: f32) -> Div {
    let diff = app.bindings.diff.clone();
    let panel = app.bindings.diff_panel.clone();
    let tab = app.bindings.right_tab.clone();
    let focus = app.bindings.diff_focus.clone();
    let header = app.bindings.header.clone();
    let dirs = app.bindings.dirs.clone();
    let cmp_inbox = composer::state::Cmp::get().inbox.clone();
    let app = app.clone();
    let handle_app = app.clone();
    let panel_w = width.get();
    let body = stateful_with_key::<NoState>("mq-right-panel")
        .deps([
            diff.signal_id(),
            panel.signal_id(),
            tab.signal_id(),
            focus.signal_id(),
            header.signal_id(),
            dirs.signal_id(),
            cmp_inbox.signal_id(),
            width.signal_id(),
        ])
        .on_state(move |_| {
            let close = app.clone();
            let queued = cmp_inbox.get().len();
            let current = tab.get();
            let head = div()
                .class("mq-right-head")
                .w_full()
                .flex_row()
                .items_center()
                .child(
                    div()
                        .class("mq-right-tabs")
                        .flex_1()
                        .min_w(0.0)
                        .flex_row()
                        .items_center()
                        .child(right_tab_chip(
                            &app,
                            RightTab::Queue,
                            app.strings.queue(),
                            Some(queued),
                        ))
                        .child(right_tab_chip(
                            &app,
                            RightTab::Git,
                            app.strings.git_tab(),
                            None,
                        )),
                )
                .child(ghost_icon(Glyph::Close).on_click(move |_| close_right(&close)));

            let content = match current {
                RightTab::Queue => queue_tab_body(&app),
                RightTab::Git => git_tab_body(&app),
            };
            div()
                .class("mq-right-body")
                .w_full()
                .h(height)
                .flex_col()
                .child(head)
                .child(div().w_full().flex_1().min_h(0.0).child(content))
        });
    div()
        .class("mq-right-column")
        .relative()
        .w(panel_w)
        .h(height)
        .flex_shrink_0()
        .child(body)
        .child(right_resize_handle(&handle_app, width, window, side))
}

fn queue_tab_body(app: &App) -> Div {
    let Some(list) = chips::queue_panel(app) else {
        return div()
            .class("mq-right-empty")
            .w_full()
            .h_full()
            .flex_col()
            .child(
                ui_text(app.strings.queue_empty())
                    .size(12.0)
                    .no_wrap()
                    .no_cursor(),
            );
    };
    div()
        .class("mq-right-queue")
        .w_full()
        .h_full()
        .child(scroll().w_full().h_full().child(list))
}

fn git_tab_body(app: &App) -> Div {
    let header = app.bindings.header.get();
    let dirs = app.bindings.dirs.get();
    let branch = header.as_ref().and_then(|info| {
        info.git_info
            .as_ref()
            .and_then(|g| g.branch.clone())
            .or_else(|| {
                let dir = info.directory.as_deref()?;
                dirs.get(dir.trim_end_matches(['/', '\\']))
                    .and_then(|d| d.branch.clone())
            })
    });
    let open = header.as_ref().map(|h| h.asid.0.clone());
    let stat = app
        .bindings
        .diff
        .get()
        .filter(|d| Some(&d.asid) == open.as_ref() && !d.paths.is_empty());
    let focused = app.bindings.diff_focus.get();

    let mut col = div()
        .class("mq-right-git")
        .w_full()
        .h_full()
        .flex_col()
        .child(
            div()
                .class("mq-right-section")
                .w_full()
                .flex_row()
                .items_center()
                .child(icons::tinted(Glyph::Branch, 14.0))
                .child(
                    div().flex_1().min_w(0.0).overflow_clip().child(
                        mono_text(branch.unwrap_or_else(|| "—".into()))
                            .size(13.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
                ),
        );

    let Some(stat) = stat else {
        return col.child(
            div()
                .class("mq-right-empty")
                .w_full()
                .flex_1()
                .child(
                    ui_text(app.strings.changed_files())
                        .size(12.0)
                        .no_wrap()
                        .no_cursor(),
                )
                .child(
                    div().class("mq-menu-dim").child(
                        ui_text(app.strings.diff_no_preview())
                            .size(12.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
                ),
        );
    };

    let mut list = div().class("mq-diff-panel-list").flex_col().w_full();
    for file in stat.paths.iter().cloned() {
        let selected = focused.as_deref() == Some(file.path.as_str());
        let toggle = app.clone();
        let path = file.path.clone();
        let open_app = app.clone();
        let open_path = file.path.clone();
        let mut row = div()
            .class("mq-diff-panel-row")
            .w_full()
            .flex_row()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w(0.0)
                    .overflow_clip()
                    .cursor_pointer()
                    .child(
                        mono_text(file.path.clone())
                            .size(12.0)
                            .no_wrap()
                            .no_cursor(),
                    )
                    .on_click(move |_| {
                        let next =
                            if toggle.bindings.diff_focus.get().as_deref() == Some(path.as_str()) {
                                None
                            } else {
                                Some(path.clone())
                            };
                        toggle.bindings.diff_focus.signal().set(next);
                    }),
            )
            .child(
                div().class("mq-diff-added").child(
                    mono_text(format!("+{}", file.added))
                        .size(11.0)
                        .no_wrap()
                        .no_cursor(),
                ),
            )
            .child(
                div().class("mq-diff-removed").child(
                    mono_text(format!("−{}", file.removed))
                        .size(11.0)
                        .no_wrap()
                        .no_cursor(),
                ),
            )
            .child(
                div()
                    .class("mq-diff-open")
                    .cursor_pointer()
                    .child(
                        ui_text(app.strings.open_file())
                            .size(11.0)
                            .no_wrap()
                            .no_cursor(),
                    )
                    .on_click(move |_| open_diff_file(&open_app, &open_path)),
            );
        if selected {
            row = row.class("mq-diff-panel-row--open");
        }
        list = list.child(row);
        if selected {
            list = list.child(diff_patch_preview(&file.patch, app));
        }
    }
    if stat.files > stat.paths.len() {
        list = list.child(
            div().class("mq-menu-dim").child(
                ui_text(format!("+{}", stat.files - stat.paths.len()))
                    .size(11.0)
                    .no_wrap()
                    .no_cursor(),
            ),
        );
    }
    col = col.child(
        div().class("mq-right-section").w_full().child(
            ui_text(app.strings.changed_files())
                .size(11.0)
                .semibold()
                .no_wrap()
                .no_cursor(),
        ),
    );
    col.child(
        div()
            .w_full()
            .flex_1()
            .min_h(0.0)
            .child(scroll().w_full().h_full().child(list)),
    )
}

const DIFF_PANEL_PREVIEW_LINES: usize = 28;

fn diff_patch_preview(patch: &str, app: &App) -> Div {
    let mut body = div()
        .class("mq-diff-preview")
        .w_full()
        .flex_col()
        .overflow_clip();
    if patch.trim().is_empty() {
        return body.child(
            div().class("mq-menu-dim").child(
                ui_text(app.strings.diff_no_preview())
                    .size(11.0)
                    .no_wrap()
                    .no_cursor(),
            ),
        );
    }
    let lines: Vec<&str> = patch
        .lines()
        .filter(|l| {
            !(l.starts_with("diff --git")
                || l.starts_with("index ")
                || l.starts_with("--- ")
                || l.starts_with("+++ ")
                || l.starts_with("new file mode")
                || l.starts_with("deleted file mode")
                || l.starts_with("similarity index")
                || l.starts_with("rename "))
        })
        .collect();
    let total = lines.len();
    for line in lines.iter().take(DIFF_PANEL_PREVIEW_LINES) {
        let class = if line.starts_with('+') {
            "mq-diff-added"
        } else if line.starts_with('-') {
            "mq-diff-removed"
        } else if line.starts_with("@@") {
            "mq-diff-hunk"
        } else {
            "mq-menu-dim"
        };
        body = body.child(
            div().class(class).w_full().overflow_clip().child(
                mono_text((*line).to_string())
                    .size(11.0)
                    .no_wrap()
                    .no_cursor(),
            ),
        );
    }
    if total > DIFF_PANEL_PREVIEW_LINES {
        body = body.child(
            div().class("mq-menu-dim").child(
                ui_text(format!("+{}…", total - DIFF_PANEL_PREVIEW_LINES))
                    .size(11.0)
                    .no_wrap()
                    .no_cursor(),
            ),
        );
    }
    body
}

/// The sidebar's width, bound to its column so a drag moves it without a
/// rebuild. Kept between 220 and 360; a double press on the edge resets it.
fn sidebar_width(app: &App) -> State<f32> {
    let width = app.config.get().ui.sidebar();
    blinc_core::BlincContextState::get().use_state_keyed("mq.sidebar.width", move || width)
}

/// Folded to the rail by Ctrl/Cmd+B (independent of the narrow-window rail).
fn sidebar_collapsed() -> State<bool> {
    blinc_core::BlincContextState::get().use_state_keyed("mq.sidebar.collapsed", || false)
}

/// Toggle the left sidebar between the full tree and the icon rail.
pub fn toggle_sidebar() {
    let state = sidebar_collapsed();
    state.signal().set(!state.get());
}

/// True while the left column is the rail (narrow window or Ctrl/Cmd+B).
pub fn is_rail(window_width: f32) -> bool {
    sidebar_collapsed().get() || window_width < RAIL_BELOW
}

/// Width the left column occupies for the current window.
pub fn side_width(app: &App, window_width: f32) -> f32 {
    if is_rail(window_width) {
        sessions::RAIL_WIDTH
    } else {
        app.config.get().ui.sidebar()
    }
}

/// (pointer x at the press, width then, when the last press was).
static DRAG: OnceLock<Mutex<(f32, f32, Option<Instant>)>> = OnceLock::new();

fn resize_handle(app: &App, width: State<f32>) -> Div {
    let drag = DRAG.get_or_init(|| Mutex::new((0.0, 0.0, None)));
    let (down_w, move_w, end_w) = (width.clone(), width.clone(), width);
    let (reset_app, end_app) = (app.clone(), app.clone());
    div()
        .class("mq-side-handle")
        .absolute()
        .top(0.0)
        .bottom(0.0)
        .right(-3.0)
        .w(6.0)
        .cursor(CursorStyle::ResizeEW)
        .on_mouse_down(move |e| {
            let mut d = drag.lock().unwrap();
            let now = Instant::now();
            if d.2.is_some_and(|last| now - last < DOUBLE_CLICK) {
                d.2 = None;
                down_w.set(UiPrefs::SIDEBAR_DEFAULT);
                reset_app.send(Command::Edit(ConfigEdit::SidebarWidth(
                    UiPrefs::SIDEBAR_DEFAULT,
                )));
                return;
            }
            *d = (e.mouse_x, down_w.get(), Some(now));
        })
        .on_drag(move |e| {
            let (x, start, _) = *drag.lock().unwrap();
            let next = (start + e.mouse_x - x).clamp(UiPrefs::SIDEBAR_MIN, UiPrefs::SIDEBAR_MAX);
            if (move_w.get() - next).abs() >= 0.5 {
                move_w.set(next);
            }
        })
        .on_drag_end(move |_| {
            end_app.send(Command::Edit(ConfigEdit::SidebarWidth(end_w.get())));
        })
}

pub fn body(ctx: &WindowedContext, app: &App) -> Div {
    // Full slot under the topbar; columns are shorter by FRAME_PAD so the
    // sidebar's bottom radius sits above the window edge.
    let slot = (ctx.height - TOP_BAR_HEIGHT).max(0.0);
    let height = (slot - FRAME_PAD).max(0.0);
    let window = ctx.width;
    let collapsed = sidebar_collapsed();
    let width = sidebar_width(app);
    let right_w = right_panel_width(app);
    let right_open = app.bindings.diff_panel.clone();
    let right_tab = app.bindings.right_tab.clone();
    let app = app.clone();
    // Rebuild when Ctrl/Cmd+B folds the left column, the right panel toggles /
    // resizes, or its tab changes.
    let body = stateful_with_key::<NoState>("mq-shell-body")
        .deps([
            collapsed.signal_id(),
            right_open.signal_id(),
            right_tab.signal_id(),
            right_w.signal_id(),
            width.signal_id(),
        ])
        .on_state(move |_| {
            let rail = collapsed.get() || window < RAIL_BELOW;
            let side = if rail {
                sessions::RAIL_WIDTH
            } else {
                width.get()
            };
            let right = if right_open.get() {
                let clamped = clamp_right_width(right_w.get(), window, side);
                if (right_w.get() - clamped).abs() >= 0.5 {
                    right_w.set(clamped);
                }
                clamped
            } else {
                0.0
            };
            // Horizontal FRAME_PAD + column gap (--mq-space-2) match the CSS.
            let main_width = (window - 2.0 * FRAME_PAD - side - right - 8.0).max(0.0);
            let column = if rail {
                div()
                    .class("mq-side-column")
                    .w(sessions::RAIL_WIDTH)
                    .h(height)
                    .flex_shrink_0()
                    .child(sessions::rail(&app, height))
            } else {
                div()
                    .class("mq-side-column")
                    .relative()
                    .w(&width)
                    .h(height)
                    .flex_shrink_0()
                    .child(sessions::sidebar(&app, height))
                    .child(resize_handle(&app, width.clone()))
            };
            let mut row = div()
                .class("mq-body")
                .w_full()
                .h(height)
                .flex_row()
                .child(column)
                .child(
                    // Absolutely placed, so the panel's width never depends on
                    // its content: a long unwrapped line in a message would
                    // otherwise push the sidebar narrower than asked.
                    div().relative().flex_1().min_w(0.0).h(height).child(
                        div()
                            .absolute()
                            .left(0.0)
                            .top(0.0)
                            .right(0.0)
                            .bottom(0.0)
                            .child(main_panel(&app, main_width, height)),
                    ),
                );
            if right_open.get() {
                row = row.child(right_column(&app, height, right_w.clone(), window, side));
            }
            row
        });
    div().w_full().h(slot).flex_col().child(body)
}

fn main_panel(app: &App, width: f32, height: f32) -> Div {
    let selected = app.bindings.selected.clone();
    let settings_open = app.bindings.settings_open.clone();
    let inbox_open = app.bindings.inbox_open.clone();
    let tasks_open = app.bindings.tasks_open.clone();
    let cmp = composer::state::Cmp::get();
    let strip = errors::strip(app);
    let plate = revert_plate(app);
    let app = app.clone();
    let right_open = app.bindings.diff_panel.clone();
    let view = stateful_with_key::<NoState>("mq-main-view")
        .deps([
            selected.signal_id(),
            settings_open.signal_id(),
            inbox_open.signal_id(),
            tasks_open.signal_id(),
            cmp.inbox.signal_id(),
            cmp.staged.signal_id(),
            cmp.rows.signal_id(),
            // Composer / timeline share this panel width; rebuild when the
            // git column opens so the input card shrinks with the column.
            right_open.signal_id(),
        ])
        .on_state(move |_| {
            if settings_open.get() {
                return settings::page(&app, width, height);
            }
            if inbox_open.get() {
                return inbox_page(&app, height);
            }
            if tasks_open.get() {
                return super::tasks::page(&app, height);
            }
            if selected.get().is_none() {
                return home::home(&app, width, height);
            }
            let timeline_height = (height - composer::reserved_height()).max(0.0);
            div()
                .w_full()
                .h(height)
                .flex_col()
                .child(timeline::timeline(&app, timeline_height))
                .child(composer::composer(&app, width))
        });
    div()
        .class("mq-main")
        .relative()
        .w_full()
        .h(height)
        .child(view)
        // Absolutely placed over the top, so neither moves the layout under
        // the reader; the alert strip goes last, above the plate.
        .child(plate)
        .child(strip)
}

/// "Revert staged · Confirm · Cancel", while the open session has a
/// rollback waiting for an answer.
fn revert_plate(app: &App) -> impl ElementBuilder {
    let header = app.bindings.header.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-revert-plate")
        .deps([header.signal_id()])
        .on_state(move |_| {
            let Some(info) = header.get().filter(|i| i.revert.is_some()) else {
                return div();
            };
            let confirm = {
                let (app, asid) = (app.clone(), info.asid.clone());
                button(app.strings.confirm())
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Small)
                    .on_click(move |_| app.send(Command::CommitRevert { asid: asid.clone() }))
            };
            let cancel = {
                let (app, asid) = (app.clone(), info.asid.clone());
                button(app.strings.cancel())
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Small)
                    .on_click(move |_| app.send(Command::ClearRevert { asid: asid.clone() }))
            };
            div()
                .class("mq-revert-plate")
                .absolute()
                .top(0.0)
                .left(0.0)
                .w_full()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .child(
                    div()
                        .flex_1()
                        .child(ui_text(app.strings.revert_staged()).size(13.0)),
                )
                .child(confirm)
                .child(cancel)
        })
}

/// The Inbox (a placeholder for its route): what waits for an answer in
/// every session, each opening its session.
fn inbox_page(app: &App, height: f32) -> Div {
    let b = app.bindings.clone();
    let strings = app.strings;
    let app = app.clone();
    let list = stateful_with_key::<NoState>("mq-inbox-list")
        .deps([b.attention.signal_id(), b.waiting_forms.signal_id()])
        .on_state(move |_| {
            let sessions = b.sessions.get();
            let title = |asid: &str| {
                sessions
                    .iter()
                    .find(|s| s.asid.0 == asid)
                    .map(|s| title_of(&app, s))
                    .unwrap_or_else(|| app.strings.untitled().to_string())
            };
            let mut waiting: Vec<(String, String)> = b
                .attention
                .get()
                .iter()
                .map(|p| (p.asid.0.clone(), p.prompt.clone()))
                .collect();
            waiting.extend(
                b.waiting_forms
                    .get()
                    .iter()
                    .map(|f| (f.asid.0.clone(), f.title.clone())),
            );
            let mut list = div().class("mq-inbox-list").w_full().flex_col();
            if waiting.is_empty() {
                return list.child(
                    div()
                        .class("mq-empty")
                        .child(ui_text(app.strings.inbox_empty()).size(13.0)),
                );
            }
            for (asid, what) in waiting {
                let click = app.clone();
                let id = muqun_gateway::agents::AgentSessionId(asid.clone());
                list = list.child(
                    div()
                        .class("mq-inbox-row")
                        .w_full()
                        .flex_row()
                        .items_center()
                        .cursor_pointer()
                        .child(sessions::badge(sessions::Badge::Waiting))
                        .child(
                            div()
                                .flex_col()
                                .flex_1()
                                .min_w(0.0)
                                .child(ui_text(title(&asid)).size(13.0).semibold().no_wrap())
                                .child(
                                    div()
                                        .class("mq-text-muted")
                                        .child(ui_text(what).size(12.0).no_wrap()),
                                ),
                        )
                        .on_click(move |_| click.open_session(&id)),
                );
            }
            list
        });
    div()
        .class("mq-inbox")
        .w_full()
        .h(height)
        .flex_col()
        .child(ui_text(strings.inbox()).size(18.0).semibold())
        .child(
            div()
                .class("mq-text-muted")
                .child(ui_text(strings.inbox_soon()).size(13.0)),
        )
        .child(
            div()
                .w_full()
                .flex_1()
                .min_h(0.0)
                .child(scroll().w_full().h_full().child(list)),
        )
}
