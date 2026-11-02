//! The New-session page (Ctrl/Cmd+N): agent, then folder, then create.
//! The defaults are filled in -- the last agent used, and that agent's last
//! folder -- so Enter alone starts a session (architecture §6.1b).
//!
//! The folder is a combobox: the agent's known projects (Recent), the folders
//! under whatever is typed, "Browse…" for the system picker, and a typed path
//! used as typed. `~` is expanded when the session is created.

use blinc_cn::components::combobox::{ComboboxBuilder, ComboboxOption};
use blinc_cn::prelude::{button, ButtonSize, ButtonVariant};
use blinc_layout::prelude::*;
use muqun_gateway::home_relative;

use super::agents::{self, Standing};
use super::text::{mono_text, ui_text};
use super::App;
use crate::model::commands::Command;

/// The chooser's state keys. `blinc_cn`'s combobox keeps its open flag, its
/// search box and the typed query under `<key>_open`, `<key>_search` and
/// `<key>_search_query` (blinc_cn 0.5.1, `combobox.rs`), which is how this
/// page reads what is being typed and closes the list from the keyboard.
const CHOOSER_KEY: &str = "mq-chooser";
const CHOOSER_OPEN: &str = "mq-chooser_open";
const CHOOSER_SEARCH: &str = "mq-chooser_search";
const CHOOSER_QUERY: &str = "mq-chooser_search_query";

const CHOOSER_WIDTH: f32 = 520.0;

/// Rows offered at once. The list is `blinc_cn`'s: it scrolls inside a box
/// whose height `styles.css` sets, but Blinc 0.5.1 paints the text of rows
/// past the box's edge outside it (the clip does not reach text drawn in the
/// foreground layer), so the list is kept short enough to fit the box and
/// narrowed by relevance instead of scrolled.
const MAX_OPTIONS: usize = 8;

/// Longest folder label a row shows, in characters. Text past the list's
/// edge is painted outside it (see [`MAX_OPTIONS`]), so long paths are
/// shortened in the middle: the start says where, the end says which.
const LABEL_CHARS: usize = 46;

/// `path` shortened to at most `max` characters with an ellipsis in the
/// middle, keeping more of the end than the start.
fn ellipsize_middle(path: &str, max: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max || max < 8 {
        return path.to_string();
    }
    let tail = (max - 1) * 2 / 3;
    let head = max - 1 - tail;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// The text typed in the chooser right now, lower-cased and trimmed.
fn typed_prefix() -> String {
    blinc_core::BlincContextState::get()
        .use_state_keyed(CHOOSER_QUERY, String::new)
        .get()
        .trim()
        .to_lowercase()
}

pub fn page(app: &App, height: f32) -> Div {
    let page = div()
        .class("mq-new-page")
        .w_full()
        .flex_col()
        .child(ui_text(app.strings.new_session()).size(20.0).semibold())
        .child(label_row(app.strings.agent()))
        .child(agent_list(app))
        .child(label_row(app.strings.folder()))
        .child(
            div()
                .class("mq-folder")
                .w_full()
                .flex_row()
                .items_center()
                .child(chooser(app))
                .child(browse_button(app)),
        )
        .child(actions(app));
    div()
        .w_full()
        .h(height)
        .child(scroll().w_full().h(height).child(page))
}

fn label_row(text: &str) -> Div {
    div()
        .class("mq-field-label")
        .child(ui_text(text.to_string()).size(12.0).semibold())
}

/// Usable agents to pick from, the rest (with the gateway's reason) greyed.
fn agent_list(app: &App) -> impl ElementBuilder {
    let draft = app.bindings.draft.clone();
    let agents_state = app.bindings.agents.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-new-agents")
        .deps([draft.signal_id(), agents_state.signal_id()])
        .on_state(move |_| {
            let Some(current) = draft.get() else {
                return div();
            };
            let agents_list = agents_state.get();
            let ranked = agents::by_standing(&agents_list);

            let mut usable = div().class("mq-pick-list").w_full().flex_col();
            let mut more = div().class("mq-pick-list").w_full().flex_col();
            for (info, standing) in &ranked {
                let chosen = info.id == current.agent;
                let mut row = div().class("mq-pick");
                if chosen {
                    row = row.class("mq-pick--active");
                }
                row = row
                    .w_full()
                    .flex_row()
                    .items_center()
                    .child(agents::mark(&info.id, 16.0))
                    .child(
                        div().class("mq-pick-name").flex_1().child(
                            ui_text(agents::name(&info.id, &agents_list))
                                .size(13.0)
                                .no_wrap()
                                .no_cursor(),
                        ),
                    );
                if *standing == Standing::Usable {
                    let pick = app.clone();
                    let id = info.id.clone();
                    usable = usable.child(row.cursor_pointer().on_click(move |_| {
                        if id
                            != pick
                                .bindings
                                .draft
                                .get()
                                .map(|d| d.agent)
                                .unwrap_or_default()
                        {
                            pick.draft_for(id.clone());
                        }
                    }));
                } else {
                    let reason = info
                        .reason
                        .clone()
                        .filter(|r| !r.is_empty())
                        .map(|r| format!(" — {r}"))
                        .unwrap_or_default();
                    more = more.child(
                        row.class("mq-pick--disabled").child(
                            ui_text(format!("{}{reason}", standing.word(&app.strings)))
                                .size(11.0)
                                .no_cursor(),
                        ),
                    );
                }
            }
            let mut list = div().w_full().flex_col().child(usable);
            if ranked.iter().any(|(_, s)| *s != Standing::Usable) {
                list = list.child(label_row(app.strings.more_agents())).child(more);
            }
            list
        })
}

/// The folder combobox: the agent's known projects (Recent; a folder that is
/// gone is greyed and says so) and, while typing, the folders under the typed
/// prefix, which the core lists after a short pause. A typed path that is in
/// neither list can still be used as typed.
fn chooser(app: &App) -> impl ElementBuilder {
    let projects = app.bindings.projects.clone();
    let draft = app.bindings.draft.clone();
    let completions = app.bindings.completions.clone();
    let value = app.bindings.chooser.clone();
    let strings = app.strings;
    stateful_with_key::<NoState>("mq-chooser-view")
        .deps([projects.signal_id(), completions.signal_id()])
        .on_state(move |_| {
            let home = dirs::home_dir();
            let shown = |path: &str| home_relative(path, home.as_deref());
            let typed = typed_prefix();
            let mut options: Vec<ComboboxOption> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            // What is typed, if anything, narrows the recents the way the
            // list itself will; folders found under it come first.
            for entry in completions.get().iter() {
                if options.len() < MAX_OPTIONS && seen.insert(entry.path.clone()) {
                    options.push(folder_option(
                        &entry.path,
                        shown(&entry.path),
                        FolderTag::Found,
                    ));
                }
            }
            let known = projects.get();
            let wanted = draft.get().map(|d| d.agent).unwrap_or_default();
            let recents = known.projects.iter().filter(|_| known.agent == wanted);
            for project in recents {
                if options.len() >= MAX_OPTIONS {
                    break;
                }
                let label = shown(&project.canonical);
                let matches = typed.is_empty()
                    || label.to_lowercase().contains(&typed)
                    || project.canonical.to_lowercase().contains(&typed);
                if matches && seen.insert(project.canonical.clone()) {
                    options.push(folder_option(
                        &project.canonical,
                        label,
                        FolderTag::Recent {
                            missing: project.missing,
                            note: strings.folder_missing(),
                            recent: strings.recent(),
                        },
                    ));
                }
            }
            div().child(
                ComboboxBuilder::with_key(CHOOSER_KEY, &value)
                    .options(options)
                    .placeholder(strings.folder_placeholder())
                    .allow_custom(true)
                    .w(CHOOSER_WIDTH),
            )
        })
}

enum FolderTag {
    Recent {
        missing: bool,
        note: &'static str,
        recent: &'static str,
    },
    Found,
}

fn folder_option(value: &str, label: String, tag: FolderTag) -> ComboboxOption {
    let (missing, side) = match &tag {
        FolderTag::Recent {
            missing,
            note,
            recent,
        } => (*missing, Some(if *missing { *note } else { *recent })),
        FolderTag::Found => (false, None),
    };
    let text = ellipsize_middle(&label, LABEL_CHARS);
    let option = ComboboxOption::new(value, label).content(move || {
        let mut row = div()
            .class("mq-folder-option")
            .w_full()
            .flex_row()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w(0.0)
                    .overflow_clip()
                    .child(mono_text(text.clone()).size(13.0).no_wrap().no_cursor()),
            );
        if let Some(side) = side {
            row = row.child(
                div()
                    .class("mq-folder-tag")
                    .child(ui_text(side).size(11.0).no_wrap().no_cursor()),
            );
        }
        if missing {
            row = row.class("mq-folder-option--missing");
        }
        row
    });
    if missing {
        option.disabled()
    } else {
        option
    }
}

fn browse_button(app: &App) -> impl ElementBuilder {
    let app = app.clone();
    button(app.strings.browse())
        .variant(ButtonVariant::Outline)
        .size(ButtonSize::Small)
        .on_click(move |_| pick_folder(&app))
}

fn actions(app: &App) -> impl ElementBuilder {
    let draft = app.bindings.draft.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-new-actions")
        .deps([draft.signal_id()])
        .on_state(move |_| {
            let create = {
                let app = app.clone();
                button(app.strings.create())
                    .size(ButtonSize::Small)
                    .on_click(move |_| app.create_from_draft())
            };
            let cancel = {
                let app = app.clone();
                button(app.strings.cancel())
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Small)
                    .on_click(move |_| app.bindings.draft.signal().set(None))
            };
            div()
                .class("mq-new-actions")
                .flex_row()
                .items_center()
                .child(create)
                .child(cancel)
                .child(ui_text(app.strings.new_session_hint()).size(12.0))
        })
}

/// Watch what is typed in the chooser and ask the core for the folders under
/// it. The core waits for typing to pause and answers only for the newest
/// text, so every keystroke may send this; nothing here waits for a reply.
/// Called once, from the first build.
pub fn watch_typed_folder(app: &App) {
    let query = blinc_core::BlincContextState::get()
        .use_state_keyed(CHOOSER_QUERY, String::new)
        .signal();
    let tx = app.commands.clone();
    blinc_core::reactive::effect(move |graph| {
        let prefix = graph.get(query).unwrap_or_default();
        crate::model::commands::send(&tx, Command::CompleteDirectory { prefix });
    });
}

/// Close the chooser's list if it is open (Esc). True when it was.
pub fn close_chooser() -> bool {
    let ctx = blinc_core::BlincContextState::get();
    let open = ctx.use_state_keyed(CHOOSER_OPEN, || false);
    if !open.get() {
        return false;
    }
    clear_search();
    open.set(false);
    true
}

fn clear_search() {
    let ctx = blinc_core::BlincContextState::get();
    let data = ctx
        .use_state_keyed(
            CHOOSER_SEARCH,
            blinc_layout::widgets::text_input::text_input_data,
        )
        .get();
    if let Ok(mut data) = data.lock() {
        data.value.clear();
        data.cursor = 0;
    }
    ctx.use_state_keyed(CHOOSER_QUERY, String::new)
        .set(String::new());
}

/// Enter in the chooser with text typed: that text becomes the folder (the
/// list closes). False when nothing was typed, so Enter means "create".
pub fn accept_typed_folder(app: &App) -> bool {
    let ctx = blinc_core::BlincContextState::get();
    let typed = ctx.use_state_keyed(CHOOSER_QUERY, String::new).get();
    let typed = typed.trim();
    if typed.is_empty() {
        return false;
    }
    app.bindings.chooser.set(typed.to_string());
    clear_search();
    ctx.use_state_keyed(CHOOSER_OPEN, || false).set(false);
    true
}

/// The system folder picker; the choice goes into the chooser. The
/// dialog blocks its caller: on macOS it must run on the main thread,
/// elsewhere it runs beside the UI so frames keep coming. Either way the
/// chooser is written from the main thread by the drain's next run, through
/// the inbox -- never from the picker's thread.
fn pick_folder(app: &App) {
    let title = app.strings.choose_folder();
    let drain = app.drain.clone();
    let agent = app
        .bindings
        .draft
        .get()
        .map(|d| d.agent)
        .unwrap_or_default();
    let raw = app.bindings.chooser.get();
    let start = muqun_gateway::agents::directories::expand_home(raw.trim())
        .filter(|p| p.is_dir())
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    let pick = move || {
        let picked = blinc_app::dialog::pick_folder()
            .title(title)
            .directory(start)
            .pick();
        if let Some(cwd) = picked {
            drain.post(crate::model::inbox::UiAction::SetDraft { agent, cwd });
        }
    };
    if crate::platform::shell::dialogs_need_main_thread() {
        pick();
    } else {
        std::thread::spawn(pick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_paths_keep_their_start_and_their_end() {
        let short = "~/Work/muqun";
        assert_eq!(ellipsize_middle(short, 46), short);
        let long =
            "~/.cache/tmp/claude-1000/-home-ryu-Work-muqun/5c9e989a/scratchpad/t3-0.1.5/workspace";
        let cut = ellipsize_middle(long, 46);
        assert_eq!(cut.chars().count(), 46);
        assert!(cut.starts_with("~/.cache/tmp/"), "{cut}");
        assert!(cut.ends_with("/t3-0.1.5/workspace"), "{cut}");
        assert!(cut.contains('…'));
        // Characters, not bytes: a CJK path never splits inside one.
        let cjk = "~/工作/".to_string() + &"项目".repeat(30);
        assert_eq!(ellipsize_middle(&cjk, 20).chars().count(), 20);
    }
}
