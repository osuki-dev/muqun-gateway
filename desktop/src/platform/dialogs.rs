//! Native file and folder dialogs (`rfd`).
//!
//! A dialog blocks its caller until the owner answers. On macOS it must run
//! on the main (UI) thread; everywhere else it runs on a thread of its own
//! so frames keep coming while it is up ([`super::shell::dialogs_need_main_thread`]).
//! The answer reaches the UI one of two ways: a callback the caller passes
//! (which typically sends a [`crate::model::commands::Command`]), or
//! [`post`], which hands a [`UiAction`] to the inbox so the main thread folds
//! it in like any other core update.
//!
//! Nothing here runs when the owner cancels: `done` is simply not called.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::model::inbox::{Inbox, UiAction, Update};

static INBOX: OnceLock<Arc<Inbox>> = OnceLock::new();

/// Remember the inbox [`post`] writes to. Called once, at startup.
pub fn install(inbox: Arc<Inbox>) {
    let _ = INBOX.set(inbox);
}

/// Hand `action` to the UI thread through the inbox.
pub fn post(action: UiAction) {
    match INBOX.get() {
        Some(inbox) => inbox.push(Update::Ui(action)),
        None => tracing::warn!("a dialog answered before the inbox was installed"),
    }
}

/// Run `job` where the platform wants a dialog to run.
fn run(job: impl FnOnce() + Send + 'static) {
    if super::shell::dialogs_need_main_thread() {
        job();
    } else if let Err(err) = std::thread::Builder::new()
        .name("muqun-dialog".into())
        .spawn(job)
    {
        tracing::warn!("could not start a dialog thread: {err}");
    }
}

fn dialog(title: &str, start: Option<PathBuf>) -> rfd::FileDialog {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(dir) = start.filter(|d| d.is_dir()).or_else(dirs::home_dir) {
        dialog = dialog.set_directory(dir);
    }
    dialog
}

/// Ask for any number of files. `done` gets the non-empty answer.
pub fn pick_files(
    title: &str,
    start: Option<PathBuf>,
    done: impl FnOnce(Vec<PathBuf>) + Send + 'static,
) {
    let dialog = dialog(title, start);
    run(move || {
        if let Some(files) = dialog.pick_files().filter(|f| !f.is_empty()) {
            done(files);
        }
    });
}

/// Ask for one file, optionally of one kind (`("Muqun theme", &["muqun-theme"])`).
pub fn pick_file(
    title: &str,
    filter: Option<(&str, &[&str])>,
    start: Option<PathBuf>,
    done: impl FnOnce(PathBuf) + Send + 'static,
) {
    let mut dialog = dialog(title, start);
    if let Some((name, extensions)) = filter {
        dialog = dialog.add_filter(name, extensions);
    }
    run(move || {
        if let Some(file) = dialog.pick_file() {
            done(file);
        }
    });
}

/// Ask for one folder.
pub fn pick_folder(
    title: &str,
    start: Option<PathBuf>,
    done: impl FnOnce(PathBuf) + Send + 'static,
) {
    let dialog = dialog(title, start);
    run(move || {
        if let Some(folder) = dialog.pick_folder() {
            done(folder);
        }
    });
}

/// The composer's Attach: files come back as [`UiAction::FilesPicked`].
pub fn attach_files(start: Option<PathBuf>) {
    pick_files("Attach files", start, |files| {
        post(UiAction::FilesPicked(files));
    });
}

/// The start page's Choose folder: it comes back as [`UiAction::ProjectPicked`].
pub fn choose_project(start: Option<PathBuf>) {
    pick_folder("Choose a project folder", start, |folder| {
        post(UiAction::ProjectPicked(folder));
    });
}

/// Settings → Themes → Import file: it comes back as [`UiAction::ThemeFilePicked`].
pub fn import_theme_file() {
    pick_file(
        "Import a theme",
        Some(("Muqun theme", &["muqun-theme", "zip"])),
        None,
        |file| post(UiAction::ThemeFilePicked(file)),
    );
}

/// The viewer's Save as…: ask where, then copy `source` there. The outcome
/// comes back as [`UiAction::FileSaved`].
pub fn save_copy(source: PathBuf) {
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dialog = rfd::FileDialog::new()
        .set_title("Save a copy")
        .set_file_name(name)
        .set_directory(
            dirs::download_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(|| PathBuf::from("/")),
        );
    run(move || {
        let Some(target) = dialog.save_file() else {
            return;
        };
        let result = if target == source {
            Ok(target)
        } else {
            std::fs::copy(&source, &target)
                .map(|_| target)
                .map_err(|err| err.to_string())
        };
        post(UiAction::FileSaved(result));
    });
}
