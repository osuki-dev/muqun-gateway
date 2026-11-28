//! Desktop notifications can contain multi-line provider diagnostics.
use rune::{col, el, row, text, Color, El};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, ToastRegion};

use super::{copy, ToastLevel, UiState};
use crate::{config::Language, model::commands::Command};

fn notice(
    k: &Kit,
    key: String,
    message: &str,
    detail: Option<&str>,
    color: Color,
    language: Language,
    dismiss: impl Fn(&mut UiState) + 'static,
) -> El {
    let mut body = col().w(0.0).grow(1.0).min_w(0.0).gap(4.0).child(
        text(message)
            .font(k.ty.ui_medium)
            .color(k.c.text_base)
            .w_full()
            .wrap_anywhere(),
    );
    if let Some(detail) = detail {
        body = body.child(
            text(detail)
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .w_full()
                .wrap_anywhere(),
        );
    }
    let (width, max_body_height) = rune::memo::layout_with("shell", |r| {
        (
            (r[2] - 48.0).clamp(1.0, 420.0),
            (r[3] - 148.0).clamp(40.0, 260.0),
        )
    })
    .unwrap_or((320.0, 220.0));
    let mut notice = col()
        .key(key)
        .role("status")
        .label(message)
        .description(detail.unwrap_or_default())
        .w(width)
        .gap(8.0)
        .padding(12.0)
        .radius(k.radius.lg)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_muted)
        .child(
            row()
                .w_full()
                .items_start()
                .gap(10.0)
                .child(
                    el().size(16.0, 20.0)
                        .center()
                        .child(el().size(8.0, 8.0).radius(4.0).fill(color))
                        .a11y_hidden(),
                )
                .child(
                    body.max_h(max_body_height)
                        .scroll(rune::ScrollSpec {
                            stick_bottom: false,
                        })
                        .a11y_hidden(),
                )
                .child(
                    super::icons::header_button(k.glyph.close, copy::tr_ref(language, "Close"))
                        .on_press(dismiss)
                        .build(k),
                ),
        );
    for shadow in k.shadow_layers(&k.elevation.floating) {
        notice = notice.box_shadow(shadow);
    }
    notice
        .transition(rune::Transition::move_edge(rune::Edge::Bottom))
        .motion(rune::Spring::new(0.28, 0.0))
}

fn action(k: &Kit, label: String, act: impl Fn(&mut UiState) + 'static) -> El {
    row().pl(26.0).child(
        Button::new(label)
            .variant(ButtonVariant::Neutral)
            .control_size(ControlSize::Regular)
            .on_press(act)
            .build(k),
    )
}

pub(super) fn region(s: &UiState, k: &Kit) -> El {
    let mut region = ToastRegion::new();
    let language = s.cfg.general.language;
    if let Some(err) = &s.error {
        let mut item = notice(
            k,
            "alert-bar".into(),
            &err.message,
            err.detail.as_deref(),
            k.c.danger.icon,
            language,
            |s| {
                s.error = None;
                s.error_at = None;
            },
        );
        if err.unreachable {
            item = item.child(action(k, copy::tr(language, "Retry"), |s| {
                s.send(Command::Refresh)
            }));
        }
        region = region.toast(item);
    }
    for t in &s.toasts {
        let ttl = if t.level == ToastLevel::Error {
            5000
        } else {
            3500
        };
        if t.created_at.elapsed() >= std::time::Duration::from_millis(ttl) {
            continue;
        }
        let id = t.id;
        let color = match t.level {
            ToastLevel::Info => k.c.info.icon,
            ToastLevel::Success => k.c.success.icon,
            ToastLevel::Warning => k.c.warning.icon,
            ToastLevel::Error => k.c.danger.icon,
        };
        let mut item = notice(
            k,
            format!("toast-{id}"),
            &t.title,
            t.detail.as_deref(),
            color,
            language,
            move |s| s.toasts.retain(|t| t.id != id),
        );
        if let Some(target) = &t.target {
            let label = if target.starts_with("diagnostics:") {
                "View logs"
            } else {
                "Open"
            };
            let target = target.clone();
            item = item.child(action(k, copy::tr(language, label), move |s| {
                s.open_notification(&target);
                s.toasts.retain(|t| t.id != id);
            }));
        }
        region = region.toast(item);
    }
    region.build(k)
}
