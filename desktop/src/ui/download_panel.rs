//! Shared native progress and controls for managed downloads.
use super::{parts::glyphs, UiState};
use crate::model::viewer::human_size;
use rune::{col, el, icon, row, text, El};
use rune_kit::Kit;
#[derive(Clone, Copy)]
pub(super) enum DownloadProgress {
    Busy,
    Verifying,
    Complete,
    Bytes { received: u64, total: u64 },
}
pub(super) struct DownloadPanel<'a> {
    pub title: &'a str,
    pub message: &'a str,
    pub progress: Option<DownloadProgress>,
    pub secondary: Option<El>,
    pub primary: Option<El>,
}
// A luminous travelling band is clipped inside the measured portion of the track.
// Indeterminate phases use a moving segment instead of inventing a percentage.
fn meter(k: &Kit, ratio: Option<f32>, motion: bool, label: &str) -> El {
    let accent = k.c.bg_accent;
    let tail = accent.mix(k.c.bg_base, 0.30);
    let tip = accent.mix(k.c.white, 0.42);
    let mut track = row()
        .key("download-track")
        .role("progressbar")
        .label(label)
        .w_full()
        .h(12.0)
        .radius(6.0)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_weak);
    let mut beam = el()
        .key("download-beam")
        .h_full()
        .radius(6.0)
        .linear_gradient(1.57, [(0.0, tail), (0.68, accent), (1.0, tip)])
        .shadow(0.0, 8.0, accent.alpha(0.45));
    if let Some(ratio) = ratio {
        let v = ratio.clamp(0.0, 1.0);
        track = track
            .range_value(f64::from(v), 0.0, 1.0)
            .a11y_value(format!("{:.0}%", (v * 100.0).floor()));
        if v == 0.0 {
            return track;
        }
        if motion {
            let sweep = el()
                .w_frac(0.32)
                .h_full()
                .linear_gradient(
                    1.57,
                    [
                        (0.0, tip.alpha(0.0)),
                        (0.64, tip.alpha(0.65)),
                        (0.86, k.c.white.alpha(0.92)),
                        (1.0, tip.alpha(0.0)),
                    ],
                )
                .looping(rune::LoopSpec::slide(1.55, [-1.0, 0.0], [3.15, 0.0]));
            beam = beam.child(sweep).token_motion(k.motion.disclosure);
        }
        return track.child(
            beam.w_frac(v).clip_soft(0.0).child(
                el().pin(None, Some(2.0), Some(1.0), Some(2.0))
                    .w(2.0)
                    .radius(1.0)
                    .fill(k.c.white),
            ),
        );
    }
    beam = beam.w_frac(0.27);
    if motion {
        beam = beam.looping(rune::LoopSpec::slide(1.65, [-1.0, 0.0], [3.71, 0.0]));
    }
    track.clip_soft(0.0).child(beam)
}

impl DownloadPanel<'_> {
    pub(super) fn mount(
        self,
        s: &UiState,
        k: &Kit,
        layer: &mut Vec<El>,
        on_close: fn(&mut UiState),
    ) {
        self.render(s, k, layer, Some(on_close));
    }
    pub(super) fn mount_required(self, s: &UiState, k: &Kit, layer: &mut Vec<El>) {
        self.render(s, k, layer, None);
    }
    fn render(self, s: &UiState, k: &Kit, layer: &mut Vec<El>, on_close: Option<fn(&mut UiState)>) {
        let motion = !(s.cfg.appearance.reduced_motion || s.system_reduced_motion.get());
        let complete = matches!(self.progress, Some(DownloadProgress::Complete));
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
                            .child(
                                icon(
                                    if complete {
                                        &glyphs::DONE
                                    } else if matches!(
                                        self.progress,
                                        Some(DownloadProgress::Verifying)
                                    ) {
                                        &glyphs::CHOICE
                                    } else {
                                        &glyphs::COMPUTER
                                    },
                                    23.0,
                                )
                                .color(k.c.text_accent),
                            ),
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
                    let done = received.min(total);
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
            if complete {
                ratio = Some(1.0);
            }
            let mut panel = col()
                .w_full()
                .key("download-progress")
                .px(16.0)
                .py(14.0)
                .gap(9.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_01)
                .child(detail);
            panel = panel.child(meter(k, ratio, motion && !complete, body));
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
            .dialog_motion(look.floating_surface(k, card.child(buttons), k.radius.xl))
            .modal(on_close.is_some(), on_close.unwrap_or(|_| {}));
        layer.push(look.modal_scrim(k, "download-panel-scrim"));
        layer.push(
            el().key("download-panel-layer")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .px(18.0)
                .center()
                .child(card),
        );
    }
}
