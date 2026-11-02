//! The non-modal alert strip at the top of the main panel: the last error,
//! until dismissed or answered (a list that arrives clears "not reachable").

use blinc_cn::prelude::{button, ButtonSize, ButtonVariant};
use blinc_layout::prelude::*;

use super::text::{mono_text, ui_text};
use super::App;

pub fn strip(app: &App) -> impl ElementBuilder {
    let error = app.bindings.error.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-error-strip")
        .deps([error.signal_id()])
        .on_state(move |_| {
            let Some(error) = error.get() else {
                return div();
            };
            let dismiss = {
                let app = app.clone();
                button(app.strings.dismiss())
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Small)
                    .on_click(move |_| app.drain.dismiss_error())
            };
            let mut body = div()
                .class("mq-alert-body")
                .flex_1()
                .flex_col()
                .child(ui_text(error.message.clone()).size(13.0));
            if let Some(detail) = error.detail {
                body = body.child(mono_text(detail).size(12.0));
            }
            div()
                .class("mq-alert")
                .absolute()
                .top(0.0)
                .left(0.0)
                .w_full()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .child(body)
                .child(dismiss)
        })
}
