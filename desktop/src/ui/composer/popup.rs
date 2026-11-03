//! The `@` / `/` / `$` list: it hangs above the card while the caret is in a
//! trigger word. ↑ ↓ move, Enter or Tab take, Esc closes, typing filters
//! (all decided in `mod.rs`; this only draws the rows and takes clicks).
//!
//! It is an overlay, not a child of the card: text of the page behind is
//! drawn above any backdrop that is not in the overlay layer, so a list
//! hanging over the page's title would be written through.

use blinc_layout::prelude::*;
use blinc_layout::widgets::overlay::AnchorDirection;
use blinc_layout::widgets::overlay_stack::{OverlayBuilder, OverlayHandle};

use super::state::Cmp;
use super::triggers::VISIBLE_ROWS;
use super::{pick, popup_rows};
use crate::ui::text::{mono_text, ui_text};
use crate::ui::App;

const ROW_HEIGHT: f32 = 32.0;
/// Panel padding and border, top and bottom together.
const CHROME: f32 = 10.0;
/// Clear space between the panel and the card.
const GAP: f32 = 6.0;

/// Open, close or leave alone the overlay, as the popup state wants: open
/// while the caret is in a trigger word that has rows to show.
pub fn sync(app: &App) {
    let cmp = Cmp::get();
    let popup = cmp.popup.get();
    let rows = popup_rows(app, &popup).len();
    let want = popup.trigger.is_some() && rows > 0;
    let shown = rows.min(VISIBLE_ROWS);
    let live = cmp
        .popup_handle
        .get()
        .map(OverlayHandle::from_raw)
        .filter(OverlayHandle::is_live);
    match (want, live) {
        (true, None) => open(app, shown),
        // An overlay is placed by its top edge, so a list that grows or
        // shrinks upward is opened again at its new height.
        (true, Some(handle)) if cmp.popup_shown.get() != shown => {
            handle.close();
            open(app, shown);
        }
        (false, Some(handle)) => {
            handle.close();
        }
        _ => {}
    }
}

fn open(app: &App, shown: usize) {
    let cmp = Cmp::get();
    // The registry's bounds are parent-local, so the card's corner is worked
    // out from the layout the pages give it; its height is the registry's.
    let Some(card_height) = blinc_layout::selector::query("mq-cmp-card")
        .and_then(|c| c.bounds())
        .map(|b| b.height)
    else {
        return;
    };
    let (x, top, width) = origin(app, card_height);
    let height = shown as f32 * ROW_HEIGHT + CHROME;
    let list_app = app.clone();
    let closed = cmp.clone();
    let handle = OverlayBuilder::popover()
        .at(x, top - GAP - height)
        .size(width, height)
        .anchor_direction(AnchorDirection::Bottom)
        .on_close(move |_| closed.popup_handle.signal().set(None))
        .content(move || div().w(width).child(list(&list_app)))
        .show();
    cmp.popup_handle.signal().set(Some(handle.raw()));
    cmp.popup_shown.signal().set(shown);
}

/// The rows, redrawn as the popup, the files found or the catalogue change.
fn list(app: &App) -> impl ElementBuilder {
    let cmp = Cmp::get();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-cmp-popup")
        .deps([
            cmp.popup.signal_id(),
            cmp.files.signal_id(),
            cmp.peers.signal_id(),
            cmp.catalogs.signal_id(),
        ])
        .on_state(move |_| {
            let popup = Cmp::get().popup.get();
            let rows = popup_rows(&app, &popup);
            if popup.trigger.is_none() || rows.is_empty() {
                return div();
            }
            let selected = popup.selected.min(rows.len() - 1);
            let shown = rows.len().min(VISIBLE_ROWS);
            let first = selected.saturating_sub(VISIBLE_ROWS - 1);
            let mut panel = div().class("mq-popup").w_full().flex_col();
            for (offset, row) in rows.iter().enumerate().skip(first).take(shown) {
                let index = offset;
                let click = app.clone();
                let mut line = div()
                    .class("mq-popup-row")
                    .w_full()
                    .h(ROW_HEIGHT)
                    .flex_row()
                    .items_center()
                    .cursor_pointer()
                    .child(
                        div().flex_shrink_0().child(
                            mono_text(row.label.clone())
                                .size(12.0)
                                .no_wrap()
                                .no_cursor(),
                        ),
                    )
                    .child(
                        div()
                            .class("mq-popup-detail")
                            .flex_1()
                            .min_w(0.0)
                            .overflow_clip()
                            .child(ui_text(row.detail.clone()).size(12.0).no_wrap().no_cursor()),
                    );
                if index == selected && row.selectable {
                    line = line.class("mq-popup-row--sel");
                }
                if !row.selectable {
                    line = line.class("mq-popup-detail");
                }
                panel = panel.child(if row.selectable {
                    line.on_click(move |_| {
                        pick(&click, index);
                        super::focus_input(&click);
                    })
                } else {
                    line
                });
            }
            panel
        })
}

/// The card's top-left corner in the window and its width.
fn origin(app: &App, card_height: f32) -> (f32, f32, f32) {
    let on_start = super::state::current_surface().as_deref() == Some(crate::config::START_DRAFT);
    if on_start {
        return crate::ui::home::start_card_origin(app);
    }
    let (vw, vh) = blinc_core::BlincContextState::get().viewport_size();
    let left = crate::ui::shell::FRAME_PAD + crate::ui::shell::side_width(app, vw) + 8.0;
    let width = super::session_card_width(app);
    let panel = (vw
        - 2.0 * crate::ui::shell::FRAME_PAD
        - crate::ui::shell::side_width(app, vw)
        - crate::ui::shell::right_width(app)
        - 8.0)
        .max(0.0);
    (
        left + (panel - width) / 2.0,
        vh - super::SLOT_BOTTOM - card_height,
        width,
    )
}
