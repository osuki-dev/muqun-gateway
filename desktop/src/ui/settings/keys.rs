//! Settings → Shortcuts: every chord the window answers to (`ui/keymap.rs`,
//! the same rows the shortcuts sheet lists), read only, under what the
//! keymap check finds.

use rune::{col, el, text, El};
use rune_kit::Kit;

use super::{group, status, Tone};
use crate::ui::copy::{tr, tr_ref};
use crate::ui::UiState;

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let (clashes, shadows) = crate::ui::shortcuts::conflicts(language);
    let mut check = col().key("shortcuts-check").w_full().gap(6.0);
    if clashes.is_empty() {
        check = check.child(status(
            k,
            "sc-check-ok",
            Tone::Good,
            tr(language, "No conflicts: every chord does one thing."),
        ));
    }
    for (i, words) in clashes.into_iter().enumerate() {
        check = check.child(status(k, &format!("sc-clash-{i}"), Tone::Bad, words));
    }
    let mut out = vec![check];
    for (g, title, rows) in crate::ui::shortcuts::groups(s, k, "", "sc") {
        // The sheet's rows, on this page's row height.
        let rows = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| el().key(format!("r{i}")).w_full().py(7.0).child(r))
            .collect();
        out.push(group(k, &format!("shortcuts-{g:?}"), &title, rows));
    }
    let notes: Vec<El> = shadows
        .into_iter()
        .chain(crate::ui::shortcuts::field_notes(language))
        .enumerate()
        .map(|(i, words)| {
            text(words)
                .key(format!("sc-note-{i}"))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .w_full()
                .py(8.0)
        })
        .collect();
    if !notes.is_empty() {
        out.push(group(
            k,
            "shortcuts-notes",
            tr_ref(language, "Good to know"),
            notes,
        ));
    }
    out
}
