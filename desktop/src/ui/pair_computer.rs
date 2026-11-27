//! Computer Pairing Modal: shown when pairing a remote computer by code or access token.

use rune::{
    col, el, icon, row, spacer, text, text_input, Color, Cursor, El, Key, KeyEvent, Shadow,
    TextEdit, TextStyle, Tx,
};
use rune_kit::{Button, ButtonVariant, Kit, Loader, TextInput};

use super::copy::{tr, tr_ref};
use super::parts::glyphs;
use super::UiState;
use crate::model::commands::Command;

const OTP_LEN: usize = 8;

/// Extract up to 8 uppercase ASCII alphanumeric characters from raw or formatted input.
fn otp_chars(raw: &str) -> Vec<char> {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .take(OTP_LEN)
        .collect()
}

fn set_otp_value(edit: &mut TextEdit, code: String) {
    let end = code.len();
    edit.text = code;
    edit.anchor = end;
    edit.focus = end;
}

/// Apply typed or pasted characters into the 8-slot OTP state.
///
/// When `replace_if_full` is true (paste), pasting a full 8-character code—or pasting while
/// already at 8 characters—replaces the existing value cleanly.
fn apply_otp_insert(s: &mut UiState, incoming: &str, replace_if_full: bool) -> bool {
    let incoming_chars = otp_chars(incoming);
    if incoming_chars.is_empty() {
        return false;
    }
    let (a, b) = s.pairing_code_input.range();
    let all_selected = a == 0 && b == s.pairing_code_input.text.len() && b > 0;
    let current = otp_chars(&s.pairing_code_input.text);
    let next: String = if all_selected
        || (replace_if_full && (incoming_chars.len() >= OTP_LEN || current.len() >= OTP_LEN))
    {
        incoming_chars.into_iter().take(OTP_LEN).collect()
    } else {
        current
            .into_iter()
            .chain(incoming_chars)
            .take(OTP_LEN)
            .collect()
    };
    set_otp_value(&mut s.pairing_code_input, next);
    s.pairing_error = None;
    true
}

fn apply_otp_backspace(s: &mut UiState, clear_all: bool) {
    let (a, b) = s.pairing_code_input.range();
    let all_selected = a == 0 && b == s.pairing_code_input.text.len() && b > 0;
    let mut chars = otp_chars(&s.pairing_code_input.text);
    if clear_all || all_selected {
        chars.clear();
    } else {
        chars.pop();
    }
    set_otp_value(&mut s.pairing_code_input, chars.into_iter().collect());
    s.pairing_error = None;
}

fn normalize_otp_field(s: &mut UiState) {
    let sanitized: String = otp_chars(&s.pairing_code_input.text).into_iter().collect();
    if sanitized != s.pairing_code_input.text {
        set_otp_value(&mut s.pairing_code_input, sanitized);
    }
}

fn submit_request(s: &mut UiState, target: &str) {
    let url = s.pairing_url_input.text.trim().to_owned();
    if url.is_empty() {
        s.pairing_error = Some(tr(s.cfg.general.language, "Please provide a gateway URL"));
        return;
    }
    if let Some((attempt, url)) = s.begin_pairing(&url) {
        s.send(Command::PairComputerRequest {
            server_id: target.to_owned(),
            attempt,
            url,
        });
    }
}

fn submit_token(s: &mut UiState, target: &str) {
    let url = s.pairing_url_input.text.trim().to_owned();
    let token = s.pairing_token_input.text.trim().to_owned();
    if url.is_empty() {
        s.pairing_error = Some(tr(s.cfg.general.language, "Please provide a gateway URL"));
        return;
    }
    if token.is_empty() {
        return;
    }
    if let Some((attempt, url)) = s.begin_pairing(&url) {
        s.send(Command::PairComputerToken {
            server_id: target.to_owned(),
            attempt,
            url,
            token,
        });
    }
}

fn submit_otp(
    s: &mut UiState,
    claim_target: &str,
    claim_url: &str,
    claim_req_id: &str,
    transport_req: bool,
) {
    if s.pairing_busy {
        return;
    }
    let final_code = crate::platform::pairing::normalize_pairing_code(&s.pairing_code_input.text);
    if final_code.len() == OTP_LEN + 1 {
        let Some((attempt, url)) = s.begin_pairing(claim_url) else {
            return;
        };
        s.send(Command::PairComputerClaim {
            server_id: claim_target.to_owned(),
            attempt,
            url,
            request_id: claim_req_id.to_owned(),
            code: final_code,
            transport_required: transport_req,
        });
    } else {
        s.pairing_error = Some(tr(
            s.cfg.general.language,
            "Enter all 8 characters to connect.",
        ));
    }
}

fn is_plain_escape(ev: &KeyEvent) -> bool {
    ev.key == Key::Escape
        && !(ev.mods.shift || ev.mods.alt || ev.mods.ctrl || ev.mods.cmd || ev.mods.logo)
}

pub fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    let Some(target_id) = &s.pairing_target else {
        return;
    };

    let look = s.look();
    let language = s.cfg.general.language;
    let [accent, accent_secondary, _] = look.highlight_colors(k);
    let busy = s.pairing_busy;
    let has_error = s.pairing_error.is_some();

    let name = if let Some(offer) = &s.pairing_offer {
        if offer.server_label.is_empty() {
            offer.server_id.clone()
        } else {
            offer.server_label.clone()
        }
    } else if target_id.is_empty() {
        tr(language, "Pair a computer")
    } else {
        s.computer_name(target_id)
    };
    let peer = s.peer_by_id(target_id);
    let reachable = peer.is_some_and(|p| p.reachable);
    let show_peer_badge = peer.is_some() || s.pairing_offer.is_some();
    let effective_url = s.pairing_url_input.text.trim().to_owned();
    let target = target_id.clone();

    let mut card = col()
        .key("pair-computer-card")
        .role("dialog")
        .label(name.clone())
        .w(504.0)
        .max_w(504.0)
        .gap(18.0)
        .px(28.0)
        .py(24.0);

    // 1. Crisp open host header (no nested card-in-card)
    let status_color = if reachable {
        k.c.success.fg
    } else {
        k.c.warning.fg
    };
    let status_dot = {
        let dot = el().w(6.0).h(6.0).radius(3.0).fill(status_color);
        if reachable {
            look.spark(dot, status_color)
        } else {
            dot
        }
    };
    let subtitle_text = if !effective_url.is_empty() {
        effective_url.clone()
    } else if peer.is_some() {
        tr(language, if reachable { "Online" } else { "Offline" })
    } else {
        tr(language, "Enter gateway address")
    };

    let device_badge = col()
        .key("pair-device-badge")
        .w(42.0)
        .h(42.0)
        .radius(k.radius.lg)
        .fill(
            k.c.layer_01
                .mix(accent, if look.dark { 0.14 } else { 0.08 }),
        )
        .border(1.0, accent.alpha(if look.dark { 0.32 } else { 0.24 }))
        .box_shadow(Shadow {
            x: 0.0,
            y: 0.0,
            blur: 16.0,
            spread: 0.0,
            color: accent.alpha(if look.dark { 0.24 } else { 0.12 }),
            inset: false,
        })
        .items_center()
        .justify_center()
        .child(icon(&glyphs::COMPUTER, 20.0).color(accent));

    let mut title_row = row().gap(8.0).center_y().child(
        text(name.clone())
            .font(TextStyle::new(16.0, 650.0).lh(21.0))
            .color(k.c.text_base),
    );
    if show_peer_badge {
        title_row = title_row.child(
            row()
                .key("pair-peer-badge")
                .px(6.0)
                .py(1.5)
                .radius(k.radius.xs)
                .fill(k.c.layer_02)
                .border(1.0, k.c.border_weak)
                .child(
                    text(tr(language, "Peer"))
                        .font(k.ty.caption())
                        .color(k.c.text_muted),
                ),
        );
    }

    let mut meta_row = row().gap(6.0).center_y();
    if peer.is_some() {
        meta_row = meta_row.child(status_dot);
    }
    meta_row = meta_row.child(
        text(subtitle_text)
            .font(k.ty.caption())
            .color(k.c.text_muted)
            .truncate()
            .grow(1.0)
            .min_w(0.0),
    );

    let header = row()
        .key("pair-header")
        .w_full()
        .center_y()
        .gap(14.0)
        .child(device_badge)
        .child(
            col()
                .gap(3.0)
                .grow(1.0)
                .min_w(0.0)
                .child(title_row)
                .child(meta_row),
        )
        .child(
            super::icons::header_button(k.glyph.close, tr(language, "Close"))
                .on_press(|s: &mut UiState| s.close_pairing())
                .build(k)
                .key("pair-close-x"),
        );

    card = card
        .child(header)
        .child(el().w_full().h(1.0).fill(k.c.border_weak.alpha(0.75)));

    // 2. Main Body: Manual Token Mode vs Code Offer (OTP) vs Stage 1 (Request Code)
    if s.pairing_manual_token_mode {
        let url_target = target.clone();
        let token_target = target.clone();
        let submit_target = target.clone();
        let token_empty = s.pairing_token_input.text.trim().is_empty();

        let mut form = col()
            .key("pair-token-form")
            .w_full()
            .gap(14.0)
            .child(url_field(s, k, language, busy, move |s| {
                submit_token(s, &url_target);
            }))
            .child(
                col()
                    .w_full()
                    .gap(6.0)
                    .child(
                        text(tr(language, "Access token"))
                            .font(k.ty.small_medium)
                            .color(k.c.text_muted),
                    )
                    .child(
                        TextInput::new(&s.pairing_token_input)
                            .secure()
                            .placeholder(tr_ref(language, "Paste access token"))
                            .label(tr_ref(language, "Access token"))
                            .large()
                            .width(None)
                            .disabled(busy)
                            .on_edit(|s: &mut UiState| &mut s.pairing_token_input)
                            .on_change(|s: &mut UiState, _: &mut Tx| {
                                s.pairing_error = None;
                            })
                            .on_submit(move |s: &mut UiState, _: &mut Tx| {
                                submit_token(s, &token_target);
                            })
                            .on_key(|s: &mut UiState, ev: &KeyEvent, _: &mut Tx| {
                                if is_plain_escape(ev) {
                                    s.close_pairing();
                                    return true;
                                }
                                false
                            })
                            .build(k)
                            .key("pair-token-input"),
                    ),
            );

        if busy {
            form = form.child(busy_strip(k, accent, tr_ref(language, "Verifying token…")));
        }
        if let Some(err) = &s.pairing_error {
            form = form.child(error_banner(k, err));
        }

        form = form.child(
            row()
                .w_full()
                .center_y()
                .gap(8.0)
                .child(
                    Button::new(tr(language, "Use pairing code"))
                        .variant(ButtonVariant::Ghost)
                        .disabled(busy)
                        .on_press(|s: &mut UiState| {
                            if s.pairing_busy {
                                return;
                            }
                            s.pairing_manual_token_mode = false;
                            s.pairing_error = None;
                            s.pairing_token_input.focused = false;
                            s.pairing_url_input.focused =
                                s.pairing_url_input.text.trim().is_empty();
                        })
                        .build(k)
                        .key("pair-toggle-to-code"),
                )
                .child(spacer())
                .child(
                    Button::new(tr(language, "Cancel"))
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut UiState| s.close_pairing())
                        .build(k)
                        .key("pair-token-cancel"),
                )
                .child(
                    Button::new(if busy {
                        tr(language, "Connecting…")
                    } else {
                        tr(language, "Connect")
                    })
                    .variant(ButtonVariant::Submit)
                    .disabled(busy || token_empty)
                    .on_press(move |s: &mut UiState| submit_token(s, &submit_target))
                    .build(k)
                    .key("pair-token-submit"),
                ),
        );

        card = card.child(form);
    } else if let Some(offer) = &s.pairing_offer {
        let claim_target = offer.server_id.clone();
        let claim_url = effective_url.clone();
        let claim_req_id = offer.request_id.clone();
        let transport_req = offer.transport_required;

        let chars = otp_chars(&s.pairing_code_input.text);
        let is_complete = chars.len() == OTP_LEN;
        let focused = s.pairing_code_input.focused;
        let (sel_a, sel_b) = s.pairing_code_input.range();
        let all_selected =
            focused && sel_a == 0 && sel_b == s.pairing_code_input.text.len() && sel_b > 0;

        let mut prompt_row = row().w_full().center_y().gap(8.0).child(
            text(tr(
                language,
                "Enter the 8-character code shown on the remote computer.",
            ))
            .font(k.ty.ui_medium)
            .color(k.c.text_base)
            .grow(1.0)
            .min_w(0.0)
            .wrap(),
        );
        if busy {
            prompt_row = prompt_row.child(
                row()
                    .key("pair-otp-status")
                    .center_y()
                    .gap(6.0)
                    .child(Loader::new().size(13.0).color(accent).build(k))
                    .child(
                        text(tr(language, "Connecting…"))
                            .font(k.ty.caption())
                            .color(accent),
                    ),
            );
        } else if is_complete {
            prompt_row = prompt_row.child(
                row()
                    .key("pair-otp-status")
                    .center_y()
                    .gap(5.0)
                    .child(icon(&glyphs::DONE, 13.0).color(k.c.success.fg))
                    .child(
                        text(tr(language, "Ready"))
                            .font(k.ty.caption())
                            .color(k.c.success.fg),
                    ),
            );
        } else if !chars.is_empty() {
            prompt_row = prompt_row.child(
                text(format!("{}/{OTP_LEN}", chars.len()))
                    .key("pair-otp-count")
                    .font(k.ty.mono)
                    .color(k.c.text_muted),
            );
        }

        let otp_strip = build_otp_input(
            s,
            &chars,
            OtpStyle {
                focused,
                busy,
                is_complete,
                all_selected,
                has_error,
                accent,
                accent_secondary,
                motion: look.motion,
                dark: look.dark,
            },
            OtpClaim {
                target: claim_target.clone(),
                url: claim_url.clone(),
                request_id: claim_req_id.clone(),
                transport_required: transport_req,
            },
            language,
            k,
        );

        let mut form = col()
            .key("pair-otp-form")
            .w_full()
            .gap(16.0)
            .child(prompt_row)
            .child(otp_strip);

        if let Some(err) = &s.pairing_error {
            form = form.child(error_banner(k, err));
        }

        let btn_target = claim_target;
        let btn_url = claim_url;
        let btn_req_id = claim_req_id;

        form = form.child(
            row()
                .w_full()
                .center_y()
                .gap(8.0)
                .child(
                    Button::new(tr(language, "Back"))
                        .variant(ButtonVariant::Ghost)
                        .disabled(busy)
                        .on_press(|s: &mut UiState| {
                            if s.pairing_busy {
                                return;
                            }
                            s.pairing_offer = None;
                            s.pairing_code_input.clear();
                            s.pairing_code_input.focused = false;
                            s.pairing_error = None;
                        })
                        .build(k)
                        .key("pair-code-cancel"),
                )
                .child(spacer())
                .child(
                    Button::new(if busy {
                        tr(language, "Connecting…")
                    } else {
                        tr(language, "Connect")
                    })
                    .variant(ButtonVariant::Submit)
                    .disabled(busy || !is_complete)
                    .on_press(move |s: &mut UiState| {
                        submit_otp(s, &btn_target, &btn_url, &btn_req_id, transport_req);
                    })
                    .build(k)
                    .key("pair-code-submit"),
                ),
        );

        card = card.child(form);
    } else {
        // Stage 1: Request Pairing Code (discovered peer or manual gateway address)
        let url_target = target.clone();
        let req_target = target.clone();
        let description = if peer.is_some() {
            tr(
                language,
                "Request a one-time pairing code on this computer to connect securely.",
            )
        } else {
            tr(
                language,
                "Enter the gateway address to request a one-time pairing code.",
            )
        };

        let mut form = col()
            .key("pair-request-form")
            .w_full()
            .gap(14.0)
            .child(
                text(description)
                    .font(k.ty.ui)
                    .color(k.c.text_muted)
                    .wrap()
                    .w_full(),
            )
            .child(url_field(s, k, language, busy, move |s| {
                submit_request(s, &url_target);
            }));

        if busy {
            form = form.child(busy_strip(
                k,
                accent,
                tr_ref(language, "Requesting pairing code…"),
            ));
        }
        if let Some(err) = &s.pairing_error {
            form = form.child(error_banner(k, err));
        }

        form = form.child(
            row()
                .w_full()
                .center_y()
                .gap(8.0)
                .child(
                    Button::new(tr(language, "Use access token"))
                        .variant(ButtonVariant::Ghost)
                        .disabled(busy)
                        .on_press(|s: &mut UiState| {
                            if s.pairing_busy {
                                return;
                            }
                            s.pairing_manual_token_mode = true;
                            s.pairing_error = None;
                            s.pairing_url_input.focused =
                                s.pairing_url_input.text.trim().is_empty();
                            s.pairing_token_input.focused = !s.pairing_url_input.focused;
                        })
                        .build(k)
                        .key("pair-toggle-to-token"),
                )
                .child(spacer())
                .child(
                    Button::new(tr(language, "Cancel"))
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut UiState| s.close_pairing())
                        .build(k)
                        .key("pair-request-cancel"),
                )
                .child(
                    Button::new(if busy {
                        tr(language, "Requesting…")
                    } else {
                        tr(language, "Request code")
                    })
                    .variant(ButtonVariant::Submit)
                    .disabled(busy)
                    .on_press(move |s: &mut UiState| submit_request(s, &req_target))
                    .build(k)
                    .key("pair-request-btn"),
                ),
        );

        card = card.child(form);
    }

    let mut panel = look.floating_surface(k, card, k.radius.xl).fill(
        k.c.surface_raised_base
            .alpha(if look.glass { 0.96 } else { 1.0 }),
    );

    if s.pairing_offer.is_some() && !s.pairing_manual_token_mode && !busy {
        // Fallback paste target if focus rests on the modal shell instead of the OTP input.
        panel = panel.on_paste(|s: &mut UiState, pasted: &str, _: &mut Tx| {
            if s.pairing_offer.is_some() && !s.pairing_manual_token_mode && !s.pairing_busy {
                apply_otp_insert(s, pasted, true);
                s.pairing_code_input.focused = true;
            }
        });
    }

    let panel = look
        .dialog_motion(panel)
        .modal(true, |s: &mut UiState| s.close_pairing());

    // Scrim overlay
    layer.push(look.modal_scrim(k, "pair-scrim"));

    // Modal dialog centered
    layer.push(
        col()
            .key("pair-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .items_center()
            .justify_center()
            .px(32.0)
            .py(48.0)
            .child(panel),
    );
}

fn url_field(
    s: &UiState,
    k: &Kit,
    language: crate::config::Language,
    busy: bool,
    on_enter: impl Fn(&mut UiState) + 'static,
) -> El {
    col()
        .w_full()
        .gap(6.0)
        .child(
            text(tr(language, "Gateway address"))
                .font(k.ty.small_medium)
                .color(k.c.text_muted),
        )
        .child(
            TextInput::new(&s.pairing_url_input)
                .placeholder(tr_ref(language, "Host or URL"))
                .label(tr_ref(language, "Gateway address"))
                .large()
                .width(None)
                .disabled(busy)
                .on_edit(|s: &mut UiState| &mut s.pairing_url_input)
                .on_change(|s: &mut UiState, _: &mut Tx| {
                    s.pairing_error = None;
                })
                .on_submit(move |s: &mut UiState, _: &mut Tx| on_enter(s))
                .on_key(|s: &mut UiState, ev: &KeyEvent, _: &mut Tx| {
                    if is_plain_escape(ev) {
                        s.close_pairing();
                        return true;
                    }
                    false
                })
                .build(k)
                .key("pair-url-input"),
        )
}

fn busy_strip(k: &Kit, accent: Color, label: &str) -> El {
    row()
        .key("pair-busy-strip")
        .w_full()
        .center_y()
        .gap(8.0)
        .px(12.0)
        .py(9.0)
        .radius(k.radius.md)
        .fill(k.c.layer_01.alpha(0.75))
        .border(1.0, k.c.border_weak)
        .child(Loader::new().size(14.0).color(accent).build(k))
        .child(text(label).font(k.ty.small).color(k.c.text_muted))
}

fn error_banner(k: &Kit, err: &str) -> El {
    row()
        .key("pair-error-banner")
        .w_full()
        .gap(10.0)
        .center_y()
        .px(13.0)
        .py(10.0)
        .radius(k.radius.md)
        .fill(k.c.danger.bg)
        .border(1.0, k.c.danger.border)
        .child(icon(&glyphs::WARNING, 15.0).color(k.c.danger.fg))
        .child(
            text(err)
                .font(k.ty.small)
                .color(k.c.danger.fg)
                .wrap()
                .grow(1.0)
                .min_w(0.0),
        )
}

#[derive(Clone, Copy)]
struct OtpStyle {
    focused: bool,
    busy: bool,
    is_complete: bool,
    all_selected: bool,
    has_error: bool,
    accent: Color,
    accent_secondary: Color,
    motion: bool,
    dark: bool,
}

#[derive(Clone)]
struct OtpClaim {
    target: String,
    url: String,
    request_id: String,
    transport_required: bool,
}

fn build_otp_input(
    s: &UiState,
    chars: &[char],
    style: OtpStyle,
    claim: OtpClaim,
    language: crate::config::Language,
    k: &Kit,
) -> El {
    let mut cells_row = row()
        .key("pin-matrix")
        .w_full()
        .center_y()
        .justify_center()
        .gap(7.0)
        .a11y_hidden();

    for i in 0..4 {
        cells_row = cells_row.child(render_otp_cell(i, chars, style, k));
    }

    let dash_color = if style.has_error {
        k.c.danger.fg.alpha(0.6)
    } else if style.is_complete {
        k.c.success.fg.alpha(0.75)
    } else if chars.len() >= 4 {
        style.accent.alpha(0.7)
    } else {
        k.c.border_strong.alpha(0.65)
    };
    cells_row = cells_row.child(
        col()
            .key("pin-divider")
            .w(24.0)
            .h(58.0)
            .items_center()
            .justify_center()
            .child(
                el().w(10.0)
                    .h(2.0)
                    .radius(1.0)
                    .fill(dash_color)
                    .token_motion(k.motion.control),
            ),
    );

    for i in 4..OTP_LEN {
        cells_row = cells_row.child(render_otp_cell(i, chars, style, k));
    }

    if style.busy {
        cells_row = cells_row.opacity(0.76);
    }

    // Invisible full-coverage text input: clicking anywhere on the 8 cells focuses it directly,
    // and typing, Backspace, Ctrl/Cmd+V paste, Ctrl+A, and Enter all flow through one control.
    let key_claim = claim.clone();
    let submit_claim = claim;
    let overlay_input = text_input(
        &s.pairing_code_input,
        TextStyle::new(24.0, 700.0).mono().lh(28.0),
    )
    .key("pair-code-input")
    .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
    .cursor(if style.busy {
        Cursor::Default
    } else {
        Cursor::Text
    })
    .single_line()
    .autofocus()
    .label(tr(language, "Pairing code"))
    .color(Color::TRANSPARENT)
    .caret(Color::TRANSPARENT, Color::TRANSPARENT)
    .placeholder("", Color::TRANSPARENT)
    .focus_ring(0.0, 0.0, Color::TRANSPARENT)
    .disabled(style.busy)
    .on_edit(|s: &mut UiState| &mut s.pairing_code_input)
    .on_input_paste(
        |s: &mut UiState, content: &rune::ClipboardContent, _: &mut Tx| {
            if !s.pairing_busy {
                apply_otp_insert(s, content.text(), true);
            }
            true
        },
    )
    .on_input_change(|s: &mut UiState, _: &mut Tx| {
        normalize_otp_field(s);
        s.pairing_error = None;
    })
    .on_submit(move |s: &mut UiState, _: &mut Tx| {
        submit_otp(
            s,
            &submit_claim.target,
            &submit_claim.url,
            &submit_claim.request_id,
            submit_claim.transport_required,
        );
    })
    .on_input_key(move |s: &mut UiState, ev: &KeyEvent, _: &mut Tx| {
        if is_plain_escape(ev) {
            s.close_pairing();
            return true;
        }
        if s.pairing_busy {
            return true;
        }
        // Let Rune handle clipboard & selection commands and Tab focus traversal.
        if matches!(ev.edit_command(), Some('v' | 'a' | 'c' | 'x')) || ev.key == Key::Tab {
            return false;
        }
        match &ev.key {
            Key::Enter => {
                submit_otp(
                    s,
                    &key_claim.target,
                    &key_claim.url,
                    &key_claim.request_id,
                    key_claim.transport_required,
                );
                true
            }
            Key::Backspace => {
                apply_otp_backspace(s, ev.mods.cmd || ev.mods.ctrl);
                true
            }
            Key::Delete => {
                let (a, b) = s.pairing_code_input.range();
                if a != b {
                    apply_otp_backspace(s, true);
                }
                true
            }
            Key::Text(t) if !(ev.mods.ctrl || ev.mods.cmd || ev.mods.alt || ev.mods.logo) => {
                let _ = apply_otp_insert(s, t, false);
                true
            }
            Key::Left | Key::Right | Key::Home | Key::End => true,
            _ => false,
        }
    });

    let stage = el()
        .key("pair-otp-stage")
        .w_full()
        .child(cells_row)
        .child(overlay_input);

    // Subtle 8-segment hairline progress strip aligned directly beneath the 4 + 4 slots
    let mut progress_row = row()
        .key("pin-progress")
        .w_full()
        .center_y()
        .justify_center()
        .gap(7.0)
        .a11y_hidden();

    for i in 0..4 {
        progress_row = progress_row.child(render_progress_segment(i, chars.len(), style, k));
    }
    progress_row = progress_row.child(el().w(24.0).h(2.5));
    for i in 4..OTP_LEN {
        progress_row = progress_row.child(render_progress_segment(i, chars.len(), style, k));
    }

    col()
        .key("pin-matrix-container")
        .w_full()
        .gap(10.0)
        .child(stage)
        .child(progress_row)
}

fn render_otp_cell(index: usize, chars: &[char], style: OtpStyle, k: &Kit) -> El {
    let ch_opt = chars.get(index).copied();
    let is_active = style.focused && !style.busy && !style.is_complete && index == chars.len();
    let slot_t = index as f32 / (OTP_LEN - 1) as f32;
    let slot_accent = style.accent.mix(style.accent_secondary, slot_t * 0.45);

    let mut cell = col()
        .key(format!("pin-cell-{index}"))
        .w(46.0)
        .h(58.0)
        .radius(k.radius.lg)
        .items_center()
        .justify_center()
        .token_motion(k.motion.control);

    if let Some(ch) = ch_opt {
        let base_fill = if style.all_selected {
            k.c.layer_01
                .mix(slot_accent, if style.dark { 0.28 } else { 0.18 })
        } else if style.has_error {
            k.c.layer_01
                .mix(k.c.danger.fg, if style.dark { 0.14 } else { 0.08 })
        } else if style.is_complete {
            k.c.layer_01
                .mix(slot_accent, if style.dark { 0.19 } else { 0.11 })
        } else {
            k.c.layer_01
                .mix(slot_accent, if style.dark { 0.13 } else { 0.07 })
        };

        let border_color = if style.has_error {
            k.c.danger.border
        } else if style.all_selected {
            slot_accent
        } else if style.is_complete {
            k.c.success.fg.alpha(if style.dark { 0.78 } else { 0.68 })
        } else {
            slot_accent.alpha(if style.dark { 0.48 } else { 0.36 })
        };

        cell = cell.fill(base_fill).border(1.5, border_color);

        if style.is_complete && !style.has_error {
            cell = cell.box_shadow(Shadow {
                x: 0.0,
                y: 0.0,
                blur: 12.0,
                spread: 0.0,
                color: k.c.success.fg.alpha(if style.dark { 0.22 } else { 0.14 }),
                inset: false,
            });
        } else {
            cell = cell.box_shadow(Shadow {
                x: 0.0,
                y: 1.5,
                blur: 4.0,
                spread: 0.0,
                color: k.c.black.alpha(if style.dark { 0.28 } else { 0.08 }),
                inset: false,
            });
        }

        cell = cell.child(
            text(ch.to_string())
                .font(TextStyle::new(24.0, 700.0).mono().lh(28.0))
                .color(if style.has_error {
                    k.c.danger.fg
                } else {
                    k.c.text_base
                }),
        );
    } else if is_active {
        let mut caret = el()
            .key("pin-caret")
            .w(2.0)
            .h(24.0)
            .radius(1.0)
            .fill(slot_accent);
        if style.motion {
            caret = caret.cycle(rune::Cycle {
                period: 1.05,
                phase: 0.0,
                stops: &[(0.0, 1.0), (0.5, 0.18), (1.0, 1.0)],
                still: 1.0,
                fps: 12.0,
            });
        }
        cell = cell
            .fill(
                k.c.layer_01
                    .mix(slot_accent, if style.dark { 0.10 } else { 0.06 }),
            )
            .border(1.5, slot_accent)
            .box_shadow(Shadow {
                x: 0.0,
                y: 0.0,
                blur: 14.0,
                spread: 0.5,
                color: slot_accent.alpha(if style.dark { 0.38 } else { 0.24 }),
                inset: false,
            })
            .box_shadow(Shadow {
                x: 0.0,
                y: 0.0,
                blur: 0.0,
                spread: 1.0,
                color: slot_accent.alpha(0.22),
                inset: true,
            })
            .child(caret);
    } else {
        cell = cell
            .fill(k.c.bg_deep.alpha(if style.dark { 0.55 } else { 0.45 }))
            .border(
                1.0,
                if style.has_error {
                    k.c.danger.border.alpha(0.75)
                } else {
                    k.c.border_weak
                },
            )
            .box_shadow(Shadow {
                x: 0.0,
                y: 1.0,
                blur: 2.0,
                spread: 0.0,
                color: k.c.black.alpha(if style.dark { 0.22 } else { 0.05 }),
                inset: true,
            });
    }

    cell
}

fn render_progress_segment(index: usize, filled_len: usize, style: OtpStyle, k: &Kit) -> El {
    let slot_t = index as f32 / (OTP_LEN - 1) as f32;
    let slot_accent = style.accent.mix(style.accent_secondary, slot_t * 0.45);
    let filled = index < filled_len;
    let active = style.focused && !style.busy && !style.is_complete && index == filled_len;

    let color = if style.has_error && filled {
        k.c.danger.fg.alpha(0.78)
    } else if style.is_complete {
        k.c.success.fg
    } else if filled {
        slot_accent
    } else if active {
        slot_accent.alpha(0.40)
    } else {
        k.c.border_weak.alpha(0.55)
    };

    let mut bar = el()
        .key(format!("pin-progress-{index}"))
        .w(46.0)
        .h(2.5)
        .radius(1.5)
        .fill(color)
        .token_motion(k.motion.control);

    if style.is_complete && !style.has_error {
        bar = bar.box_shadow(Shadow {
            x: 0.0,
            y: 0.0,
            blur: 6.0,
            spread: 0.0,
            color: k.c.success.fg.alpha(if style.dark { 0.45 } else { 0.28 }),
            inset: false,
        });
    }
    bar
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands;
    use crate::platform::pairing::PairingOffer;
    use rune::{Env, Mods, Runtime};

    fn test_view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
        let k = Kit::neutral(s.dark);
        let mut layer = Vec::new();
        mount(s, &k, &mut layer);
        rune_kit::root(el().w_full().h_full(), layer)
    }

    fn setup_offer_runtime() -> (
        Runtime<UiState>,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
    ) {
        let (tx, rx) = commands::channel();
        let mut state = UiState::new(tx);
        state.peers.push(crate::hub::PeerMention {
            server_id: "srv-remote".into(),
            name: "Studio Mac".into(),
            reachable: true,
            current: false,
            url: Some("https://studio.local:23847".into()),
        });
        state.open_pairing("srv-remote".into());
        state.pairing_url_input = TextEdit::new("https://studio.local:23847");
        state.pairing_offer = Some(PairingOffer {
            request_id: "req-42".into(),
            server_id: "srv-remote".into(),
            server_label: "Studio Mac".into(),
            expires_in_ms: 300_000,
            transport_required: true,
        });
        state.pairing_code_input.focused = true;

        let mut rt = Runtime::new(state, test_view, (960.0, 720.0), Env::new(true, 1.0));
        rt.on_key = Some(crate::ui::on_key);
        for i in 0..10 {
            rt.frame(i as f64 / 60.0);
        }
        (rt, rx)
    }

    fn text_key(ch: &str) -> KeyEvent {
        KeyEvent {
            key: Key::Text(ch.into()),
            mods: Mods::default(),
        }
    }

    fn all_rendered_text(rt: &Runtime<UiState>) -> String {
        rt.elements()
            .into_iter()
            .flat_map(|e| [e.text, e.label])
            .flatten()
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn otp_typing_auto_uppercases_supports_backspace_and_submits_on_enter() {
        let (mut rt, mut rx) = setup_offer_runtime();
        while rx.try_recv().is_ok() {}

        // Click directly on the OTP strip to confirm direct single-control focus.
        let (x, y) = rt.find("pair-code-input").expect("direct OTP input");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.2);
        assert!(rt.state.pairing_code_input.focused);

        // Type lowercase + hyphen + punctuation: hyphen & non-alnum are ignored, letters uppercased.
        for ch in ["a", "b", "1", "2", "-", " ", "c", "d", "3", "x"] {
            rt.key(text_key(ch));
        }
        rt.frame(0.3);
        assert_eq!(rt.state.pairing_code_input.text, "AB12CD3X");

        // Backspace removes the last character cleanly and lets user retype.
        rt.key(KeyEvent {
            key: Key::Backspace,
            mods: Mods::default(),
        });
        rt.frame(0.35);
        assert_eq!(rt.state.pairing_code_input.text, "AB12CD3");

        rt.key(text_key("4"));
        rt.frame(0.4);
        assert_eq!(rt.state.pairing_code_input.text, "AB12CD34");

        // Enter submits formatted XXXX-XXXX code.
        rt.key(KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        });
        rt.frame(0.45);

        assert!(rt.state.pairing_busy);
        let cmd = rx.try_recv().expect("PairComputerClaim command");
        match cmd {
            Command::PairComputerClaim {
                server_id,
                url,
                request_id,
                code,
                transport_required,
                ..
            } => {
                assert_eq!(server_id, "srv-remote");
                assert_eq!(url, "https://studio.local:23847");
                assert_eq!(request_id, "req-42");
                assert_eq!(code, "AB12-CD34");
                assert!(transport_required);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn otp_paste_strips_hyphens_and_spaces_and_uppercases() {
        let (mut rt, _rx) = setup_offer_runtime();
        let (x, y) = rt.find("pair-code-input").expect("direct OTP input");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.2);

        rt.paste("  ab12 - cd34  ");
        rt.frame(0.25);
        assert_eq!(rt.state.pairing_code_input.text, "AB12CD34");

        // Pasting a fresh full code replaces the existing 8-character code cleanly.
        rt.paste("zz99-yy88-extra");
        rt.frame(0.3);
        assert_eq!(rt.state.pairing_code_input.text, "ZZ99YY88");
    }

    #[test]
    fn otp_enter_before_complete_shows_clean_error_without_demo_placeholder() {
        let (mut rt, mut rx) = setup_offer_runtime();
        while rx.try_recv().is_ok() {}
        let (x, y) = rt.find("pair-code-input").expect("direct OTP input");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.2);

        for ch in ["a", "b", "c"] {
            rt.key(text_key(ch));
        }
        rt.key(KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        });
        rt.frame(0.25);

        assert!(!rt.state.pairing_busy);
        assert!(rx.try_recv().is_err());
        let err = rt.state.pairing_error.as_deref().expect("validation error");
        assert!(!err.contains("VDKQ"));
        assert!(!err.contains("7QUF"));

        // Typing another character clears the inline error.
        rt.key(text_key("d"));
        rt.frame(0.3);
        assert!(rt.state.pairing_error.is_none());
    }

    #[test]
    fn pairing_modal_omits_removed_crypto_banner_and_demo_clutter() {
        let (rt_otp, _rx) = setup_offer_runtime();
        let otp_dump = all_rendered_text(&rt_otp);
        for forbidden in [
            "Argon2id",
            "ChaCha20",
            "Zero-Trust",
            "VDKQ",
            "7QUF",
            "Hyphen is handled automatically",
            "Direct Input / Paste",
            "100.126.182.72",
            "P2P Link",
        ] {
            assert!(
                !otp_dump.contains(forbidden),
                "OTP modal must not render removed demo/crypto text `{forbidden}`"
            );
        }
        // Only one text input exists in OTP mode (the direct overlaid OTP control, not a second box).
        assert!(rt_otp.find("pair-code-input").is_some());
        assert!(rt_otp.find("pin-matrix").is_some());

        // Stage 1 (request screen) also has no fake IP placeholder or P2P ASCII hero.
        let (tx, _rx) = commands::channel();
        let mut s1 = UiState::new(tx);
        s1.open_manual_pairing();
        let mut rt_stage1 = Runtime::new(s1, test_view, (960.0, 720.0), Env::new(true, 1.0));
        for i in 0..8 {
            rt_stage1.frame(i as f64 / 60.0);
        }
        let stage1_dump = all_rendered_text(&rt_stage1);
        for forbidden in ["100.126.182.72", "P2P Link", "⚡", "Argon2id"] {
            assert!(
                !stage1_dump.contains(forbidden),
                "Stage 1 modal must not render demo text `{forbidden}`"
            );
        }

        // Manual token screen also has no raw "Bearer token..." developer placeholder.
        rt_stage1.state.pairing_manual_token_mode = true;
        rt_stage1.invalidate();
        for i in 8..16 {
            rt_stage1.frame(i as f64 / 60.0);
        }
        let token_dump = all_rendered_text(&rt_stage1);
        for forbidden in ["Bearer token", "100.126.182.72", "Argon2id"] {
            assert!(
                !token_dump.contains(forbidden),
                "Token mode must not render demo text `{forbidden}`"
            );
        }
    }

    #[test]
    fn stage1_to_otp_transition_autofocuses_otp_and_escape_closes_modal() {
        let (tx, mut rx) = commands::channel();
        let mut state = UiState::new(tx);
        state.open_manual_pairing();
        state.pairing_url_input = TextEdit::new("https://mini.local:23847");
        state.pairing_url_input.focused = true;

        let mut rt = Runtime::new(state, test_view, (960.0, 720.0), Env::new(true, 1.0));
        rt.on_key = Some(crate::ui::on_key);
        for i in 0..8 {
            rt.frame(i as f64 / 60.0);
        }
        while rx.try_recv().is_ok() {}

        // Submit Stage 1 via Enter in the URL input.
        rt.key(KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        });
        rt.frame(0.2);
        assert!(rt.state.pairing_busy);
        let attempt = match rx.try_recv().expect("PairComputerRequest") {
            Command::PairComputerRequest { attempt, url, .. } => {
                assert_eq!(url, "https://mini.local:23847");
                attempt
            }
            other => panic!("unexpected command: {other:?}"),
        };

        // Deliver pairing offer: OTP input must take focus immediately without clicking.
        rt.state.apply_updates(vec![
            crate::model::inbox::Update::ComputerPairingRequested {
                attempt,
                url: "https://mini.local:23847".into(),
                result: Ok(PairingOffer {
                    request_id: "req-99".into(),
                    server_id: "srv-mini".into(),
                    server_label: "Mac mini".into(),
                    expires_in_ms: 300_000,
                    transport_required: true,
                }),
            },
        ]);
        rt.invalidate();
        for i in 0..8 {
            rt.frame(0.25 + i as f64 / 60.0);
        }

        // Type directly without clicking anywhere.
        for ch in ["k", "9", "m", "2"] {
            rt.key(text_key(ch));
        }
        rt.frame(0.45);
        assert_eq!(rt.state.pairing_code_input.text, "K9M2");

        // Escape closes the pairing modal cleanly from inside the focused OTP input.
        rt.key(KeyEvent {
            key: Key::Escape,
            mods: Mods::default(),
        });
        rt.frame(0.5);
        assert!(rt.state.pairing_target.is_none());
        assert!(!rt.state.pairing_code_input.focused);
    }

    #[test]
    fn otp_clipboard_paste_shortcut_and_select_all_replace_work() {
        let (mut rt, _rx) = setup_offer_runtime();
        let clipboard = rune::clipboard::memory();
        clipboard.set_text("mn34-pq56");
        rt.use_clipboard(&clipboard);

        let (x, y) = rt.find("pair-code-input").expect("direct OTP input");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.2);

        // Ctrl/Cmd+V reads clipboard and strips hyphen + uppercases.
        let primary = if cfg!(target_os = "macos") {
            Mods {
                cmd: true,
                ..Mods::default()
            }
        } else {
            Mods {
                ctrl: true,
                ..Mods::default()
            }
        };
        rt.key(KeyEvent {
            key: Key::Command('v'),
            mods: primary,
        });
        rt.frame(0.25);
        rt.frame(0.3);
        assert_eq!(rt.state.pairing_code_input.text, "MN34PQ56");

        // Ctrl/Cmd+A then typing replaces the whole code.
        rt.key(KeyEvent {
            key: Key::Command('a'),
            mods: primary,
        });
        rt.key(text_key("q"));
        rt.frame(0.35);
        assert_eq!(rt.state.pairing_code_input.text, "Q");
    }
}
