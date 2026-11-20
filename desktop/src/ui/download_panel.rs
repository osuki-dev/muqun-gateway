//! Shared native progress and controls for managed downloads.
use super::{parts::glyphs, UiState};
use crate::model::viewer::human_size;
use rune::{col, el, icon, row, text, El};
use rune_kit::{Kit, Progress};
#[derive(Clone, Copy)]
pub(super) enum DownloadProgress {
    Busy,
    Bytes { received: u64, total: u64 },
}
pub(super) struct DownloadPanel<'a> {
    pub title: &'a str,
    pub message: &'a str,
    pub progress: Option<DownloadProgress>,
    pub secondary: Option<El>,
    pub primary: Option<El>,
}
impl DownloadPanel<'_> {
    pub(super) fn mount(
        self,
        s: &UiState,
        k: &Kit,
        layer: &mut Vec<El>,
        on_close: fn(&mut UiState),
    ) {
        if s.splash.is_some() {
            return;
        }
        let look = s.look();
        let title = self.title;
        let body = self.message;
        let mut card = col()
            .key("download-panel-card")
            .w_full()
            .max_w(450.0)
            .px(26.0)
            .py(24.0)
            .gap(18.0)
            .role("dialog")
            .label(title)
            .child(
                row()
                    .center_y()
                    .gap(14.0)
                    .child(
                        el().size(44.0, 44.0)
                            .center()
                            .radius(k.radius.lg)
                            .fill(k.c.layer_02)
                            .child(icon(&glyphs::COMPUTER, 23.0).color(k.c.text_accent)),
                    )
                    .child(
                        text(title)
                            .grow(1.0)
                            .min_w(0.0)
                            .wrap()
                            .color(k.c.text_base)
                            .font(rune::TextStyle {
                                size: 21.0,
                                line_height: 27.0,
                                ..k.ty.ui_medium
                            }),
                    ),
            );
        if let Some(progress) = self.progress {
            let mut detail = row().w_full().center_y().gap(6.0).child(
                text(body)
                    .grow(1.0)
                    .min_w(0.0)
                    .wrap()
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
            );
            let mut ratio = None;
            let mut size = None;
            if let DownloadProgress::Bytes { received, total } = progress {
                if total > 0 {
                    let done = (received).min(total);
                    ratio = Some((done as f64 / total as f64) as f32);
                    let percent = u128::from(done) * 100 / u128::from(total);
                    detail = detail.child(
                        text(format!("{percent}%"))
                            .font(k.ty.mono)
                            .color(k.c.text_base),
                    );
                    size = Some(format!("{} / {}", human_size(received), human_size(total)));
                } else {
                    size = Some(human_size(received));
                }
            }
            let motion = !(s.cfg.appearance.reduced_motion || s.system_reduced_motion.get());
            let mut panel = col()
                .w_full()
                .key("download-progress")
                .px(16.0)
                .py(14.0)
                .gap(9.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_01)
                .child(detail);
            if ratio.is_some() || motion {
                panel = panel.child(Progress::new(ratio).build(k).label(body));
            } else {
                panel = panel.child(
                    el().w_full()
                        .h(6.0)
                        .radius(k.radius.sm)
                        .fill(k.c.border_weak)
                        .role("progressbar")
                        .label(body),
                );
            }
            if let Some(size) = size {
                panel = panel.child(text(size).font(k.ty.mono).color(k.c.text_muted));
            }
            card = card.child(panel);
        } else {
            card = card.child(
                text(body)
                    .w_full()
                    .wrap()
                    .font(k.ty.body)
                    .color(k.c.text_muted),
            );
        }
        let mut buttons = row().w_full().justify_end().center_y().gap(8.0).flex_wrap();
        for button in [self.secondary, self.primary].into_iter().flatten() {
            buttons = buttons.child(button);
        }
        let card = look
            .dialog_motion(look.panel(k, card.child(buttons), k.radius.xl))
            .modal(true, on_close);
        layer.push(
            el().key("download-panel-scrim")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .fill(k.c.scrim)
                .a11y_hidden(),
        );
        layer.push(
            el().key("download-panel-layer")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .px(18.0)
                .center()
                .child(card),
        );
    }
}
