//! The window's frame (architecture §6.0‴ A): the 44 px top bar, then the
//! sidebar (a resizable 260 px column, or a 56 px rail under 960 px of
//! window / when folded with Ctrl/Cmd+B) and the main panel. A third
//! column on the right is planned; keep the main panel as the flexible
//! centre so it can sit between sidebars later.
//!
//! The main panel shows exactly one of: Settings, the Inbox, Home (the
//! start page, nothing selected), or the selected session.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use blinc_app::windowed::WindowedContext;
use blinc_cn::prelude::{dropdown_menu_custom, icons};
use blinc_core::State;
use blinc_layout::prelude::*;
use blinc_theme::{ColorScheme, ThemeState};

use super::sessions::{self, icon, title_of};
use super::text::{display_text, mono_text, ui_text};
use super::{composer, errors, home, settings, timeline, App};
use crate::branding::{mark_data_uri, APP_NAME};
use crate::config::{ConfigEdit, Scheme, UiPrefs};
use crate::model::commands::Command;
use crate::platform::editors::{self, Editor};

pub const TOP_BAR_HEIGHT: f32 = 44.0;
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

/// A 28 px ghost button holding one icon: no border, a surface on hover.
fn ghost_icon(path: &str) -> Div {
    div()
        .class("mq-icon-button")
        .w(28.0)
        .h(28.0)
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .cursor_pointer()
        .child(icon(path, 16.0))
}

pub fn top_bar(app: &App) -> Div {
    let home_app = app.clone();
    let new_app = app.clone();
    div()
        .class("mq-topbar")
        .w_full()
        .h(TOP_BAR_HEIGHT)
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .child(
            div()
                .class("mq-brand")
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .cursor_pointer()
                .child(image(mark_data_uri()).size(24.0, 24.0).rounded(6.0))
                .on_click(move |_| home_app.drain.go_home()),
        )
        .child(div().flex_1().min_w(0.0).overflow_clip().child(title(app)))
        .child(
            div()
                .class("mq-topbar-actions")
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .child(ghost_icon(icons::PLUS).on_click(move |_| new_app.open_draft()))
                .child(open_in_button(app))
                .child(diff_count(app))
                .child(ghost_icon(icons::SEARCH).on_click(|_| sessions::focus_search()))
                .child(scheme_toggle(app)),
        )
}

/// The open session's title (15 px display serif) and `⎇ branch` (12 px
/// mono); the app's name otherwise.
fn title(app: &App) -> impl ElementBuilder {
    let header = app.bindings.header.clone();
    let dirs = app.bindings.dirs.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-topbar-title")
        .deps([header.signal_id(), dirs.signal_id()])
        .on_state(move |_| {
            let line = div()
                .class("mq-topbar-title")
                .flex_row()
                .items_center()
                .min_w(0.0)
                .overflow_clip();
            let Some(info) = header.get() else {
                return line.child(
                    ui_text(APP_NAME)
                        .size(15.0)
                        .semibold()
                        .no_wrap()
                        .no_cursor(),
                );
            };
            let branch = info
                .git_info
                .as_ref()
                .and_then(|g| g.branch.clone())
                .or_else(|| {
                    let dir = info.directory.as_deref()?;
                    dirs.get()
                        .get(dir.trim_end_matches(['/', '\\']))
                        .and_then(|d| d.branch.clone())
                });
            let mut line = line.child(
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
            );
            if let Some(branch) = branch {
                line = line.child(
                    div()
                        .class("mq-topbar-branch")
                        .flex_row()
                        .items_center()
                        .flex_shrink_0()
                        .child(icon(icons::GIT_BRANCH, 12.0))
                        .child(mono_text(branch).size(12.0).no_wrap().no_cursor()),
                );
            }
            line
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
                    .child(icon(icons::CHEVRON_DOWN, 14.0))
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
                        .child(icon(icons::CODE, 14.0))
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

/// `+n −m` from `get_vcs_diff`, only while there are changes; the numbers
/// alone carry the colour. Opens the changed-files panel.
fn diff_count(app: &App) -> impl ElementBuilder {
    let diff = app.bindings.diff.clone();
    let header = app.bindings.header.clone();
    let panel = app.bindings.diff_panel.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-diff-count")
        .deps([diff.signal_id(), header.signal_id(), panel.signal_id()])
        .on_state(move |_| {
            let open = header.get().map(|h| h.asid.0.clone());
            let Some(stat) = diff
                .get()
                .filter(|d| Some(&d.asid) == open.as_ref() && d.files > 0)
            else {
                if panel.get() {
                    panel.signal().set(false);
                }
                return div();
            };
            let click = app.clone();
            let mut row = div()
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
                );
            if panel.get() {
                row = row.class("mq-diff-count--open");
            }
            row.on_click(move |_| {
                let open = !click.bindings.diff_panel.get();
                click.bindings.diff_panel.signal().set(open);
            })
        })
}

/// Overlay listing changed paths for the open session. Click a row to expand
/// its patch preview (when the agent sent one); the open icon sends it to the
/// preferred editor.
pub fn diff_overlay(app: &App) -> impl ElementBuilder {
    let diff = app.bindings.diff.clone();
    let panel = app.bindings.diff_panel.clone();
    let focus = app.bindings.diff_focus.clone();
    let header = app.bindings.header.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-diff-panel")
        .deps([
            diff.signal_id(),
            panel.signal_id(),
            focus.signal_id(),
            header.signal_id(),
        ])
        .on_state(move |_| {
            if !panel.get() {
                return div();
            }
            let open = header.get().map(|h| h.asid.0.clone());
            let Some(stat) = diff
                .get()
                .filter(|d| Some(&d.asid) == open.as_ref() && !d.paths.is_empty())
            else {
                return div();
            };
            let focused = focus.get();
            let close = app.clone();
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
                                let next = if toggle.bindings.diff_focus.get().as_deref()
                                    == Some(path.as_str())
                                {
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
                    list = list.child(diff_patch_preview(&file.patch, &app));
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
            div()
                .class("mq-diff-panel")
                .absolute()
                .right(12.0)
                .top(TOP_BAR_HEIGHT + 8.0)
                .w(420.0)
                .flex_col()
                .child(
                    div()
                        .class("mq-diff-panel-head")
                        .w_full()
                        .flex_row()
                        .items_center()
                        .child(
                            ui_text(app.strings.changed_files())
                                .size(13.0)
                                .semibold()
                                .no_wrap()
                                .no_cursor(),
                        )
                        .child(div().flex_1())
                        .child(ghost_icon(icons::X).on_click(move |_| {
                            close.bindings.diff_panel.signal().set(false);
                            close.bindings.diff_focus.signal().set(None);
                        })),
                )
                .child(
                    div()
                        .w_full()
                        .max_h(420.0)
                        .child(scroll().w_full().h_full().child(list)),
                )
        })
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

/// The moon (or sun, in light) switches the scheme; a dot on it while the
/// system's scheme is followed.
fn scheme_toggle(app: &App) -> Div {
    let following = app
        .scheme_watcher
        .lock()
        .map(|w| w.is_some())
        .unwrap_or(false);
    let path = match ThemeState::get().scheme() {
        ColorScheme::Dark => icons::MOON,
        ColorScheme::Light => icons::SUN,
    };
    let mut button = ghost_icon(path).relative();
    if following {
        button = button.child(
            div()
                .class("mq-follow-dot")
                .absolute()
                .right(5.0)
                .top(5.0)
                .w(5.0)
                .h(5.0)
                .rounded(2.5),
        );
    }
    let app = app.clone();
    button.on_click(move |_| {
        // An explicit choice stops following the system.
        let next = match ThemeState::get().scheme().toggle() {
            ColorScheme::Light => Scheme::Light,
            ColorScheme::Dark => Scheme::Dark,
        };
        app.apply_scheme(next);
    })
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
    let height = (ctx.height - TOP_BAR_HEIGHT).max(0.0);
    let window = ctx.width;
    let collapsed = sidebar_collapsed();
    let width = sidebar_width(app);
    let app = app.clone();
    // Rebuild when Ctrl/Cmd+B folds the column; drag width binds reactively.
    let body = stateful_with_key::<NoState>("mq-shell-body")
        .deps([collapsed.signal_id()])
        .on_state(move |_| {
            let rail = collapsed.get() || window < RAIL_BELOW;
            let side = if rail {
                sessions::RAIL_WIDTH
            } else {
                width.get()
            };
            let main_width = (window - side).max(0.0);
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
            div()
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
                )
        });
    div().w_full().h(height).child(body)
}

fn main_panel(app: &App, width: f32, height: f32) -> Div {
    let selected = app.bindings.selected.clone();
    let settings_open = app.bindings.settings_open.clone();
    let inbox_open = app.bindings.inbox_open.clone();
    let cmp = composer::state::Cmp::get();
    let strip = errors::strip(app);
    let app = app.clone();
    let view = stateful_with_key::<NoState>("mq-main-view")
        .deps([
            selected.signal_id(),
            settings_open.signal_id(),
            inbox_open.signal_id(),
            cmp.inbox.signal_id(),
            cmp.staged.signal_id(),
            cmp.rows.signal_id(),
        ])
        .on_state(move |_| {
            if settings_open.get() {
                return settings::page(&app, width, height);
            }
            if inbox_open.get() {
                return inbox_page(&app, height);
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
                .child(composer::composer(&app))
        });
    div()
        .class("mq-main")
        .relative()
        .w_full()
        .h(height)
        .child(view)
        // Last, and absolutely placed over the top, so the strip never
        // moves the layout under the reader.
        .child(strip)
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
