//! Computer Pairing Modal: shown when the user chooses an unpaired remote computer.

use rune::{col, icon, row, text, El, TextStyle, Tx};
use rune_kit::{Button, ButtonVariant, Kit, TextInput};

use super::copy::tr;
use super::parts::glyphs;
use super::UiState;
use crate::model::commands::Command;

pub fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    let Some(target_id) = &s.pairing_target else {
        return;
    };

    let look = s.look();
    let language = s.cfg.general.language;
    let name = s.computer_name(target_id);
    let peer = s.peer_by_id(target_id);
    let url = s.peer_url(target_id).unwrap_or_default();
    let reachable = peer.map(|p| p.reachable).unwrap_or(true);

    let effective_url = if !s.pairing_url_input.text.trim().is_empty() {
        s.pairing_url_input.text.trim().to_string()
    } else {
        url.clone()
    };

    let mut card = col()
        .key("pair-computer-card")
        .w(560.0)
        .max_w(560.0)
        .gap(18.0)
        .px(28.0)
        .py(28.0);

    // 1. Header with Device Badge, Hostname, Network Status & Dismiss
    let status_color = if reachable {
        k.c.success.fg
    } else {
        k.c.warning.fg
    };
    let status_text = if reachable {
        tr(language, "Online", "在线")
    } else {
        tr(language, "Offline", "离线")
    };

    let device_badge = col()
        .key("pair-device-badge")
        .w(48.0)
        .h(48.0)
        .radius(12.0)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_base)
        .items_center()
        .justify_center()
        .child(icon(&glyphs::COMPUTER, 24.0).color(k.c.text_base));

    let header_card = row()
        .key("pair-header-card")
        .w_full()
        .px(16.0)
        .py(14.0)
        .radius(14.0)
        .fill(k.c.layer_01.alpha(0.6))
        .border(1.0, k.c.border_weak)
        .center_y()
        .gap(14.0)
        .child(device_badge)
        .child(
            col()
                .gap(4.0)
                .grow(1.0)
                .child(
                    row()
                        .gap(8.0)
                        .center_y()
                        .child(
                            text(name.clone())
                                .font(TextStyle::new(17.0, 700.0).lh(22.0))
                                .color(k.c.text_base),
                        )
                        .child(
                            row()
                                .px(6.0)
                                .py(1.0)
                                .radius(4.0)
                                .fill(k.c.layer_01)
                                .border(1.0, k.c.border_weak)
                                .child(
                                    text(tr(language, "Peer", "对等节点"))
                                        .font(k.ty.caption())
                                        .color(k.c.text_muted),
                                ),
                        ),
                )
                .child(
                    row()
                        .gap(6.0)
                        .center_y()
                        .child(rune::el().w(7.0).h(7.0).radius(3.5).fill(status_color))
                        .child(text(status_text).font(k.ty.caption()).color(status_color))
                        .child(text("·").font(k.ty.caption()).color(k.c.text_faint))
                        .child(
                            text(if effective_url.is_empty() {
                                tr(language, "Fleet gateway", "网关舰队").to_string()
                            } else {
                                effective_url.clone()
                            })
                            .font(k.ty.caption())
                            .color(k.c.text_muted),
                        ),
                ),
        )
        .child(
            Button::new("✕")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut UiState| s.close_pairing())
                .build(k)
                .key("pair-close-x"),
        );

    card = card.child(header_card);

    // 2. If URL is missing, offer input
    if effective_url.is_empty() {
        card = card.child(
            col()
                .gap(6.0)
                .w_full()
                .child(
                    text(tr(language, "Gateway URL:", "网关地址（URL）："))
                        .font(k.ty.caption())
                        .color(k.c.text_muted),
                )
                .child(
                    TextInput::new(&s.pairing_url_input)
                        .placeholder("http://100.126.182.72:23847")
                        .on_edit(|s: &mut UiState| &mut s.pairing_url_input)
                        .build(k)
                        .key("pair-url-input"),
                ),
        );
    }

    // 3. Error banner if any
    if let Some(err) = &s.pairing_error {
        card = card.child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
                .px(14.0)
                .py(10.0)
                .radius(10.0)
                .fill(k.c.danger.bg)
                .border(1.0, k.c.danger.border)
                .child(icon(&glyphs::TRUST_WARNING, 16.0).color(k.c.danger.fg))
                .child(
                    text(err.clone())
                        .font(k.ty.small)
                        .color(k.c.danger.fg)
                        .wrap()
                        .w_full(),
                ),
        );
    }

    // 4. Main Body: Mode A (Code Pairing) vs Mode B (Manual Token)
    let busy = s.pairing_busy;
    let target = target_id.clone();
    let target_url = effective_url.clone();

    if s.pairing_manual_token_mode {
        // Mode B: Manual Token
        let token_target = target.clone();
        let token_url = target_url.clone();
        let token_val = s.pairing_token_input.text.trim().to_string();

        let form = col()
            .gap(14.0)
            .child(
                text(tr(
                    language,
                    "Enter App Token for this computer:",
                    "输入此电脑的 App 客户端凭据（Token）：",
                ))
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
            )
            .child(
                TextInput::new(&s.pairing_token_input)
                    .placeholder("Bearer token...")
                    .on_edit(|s: &mut UiState| &mut s.pairing_token_input)
                    .build(k)
                    .key("pair-token-input"),
            )
            .child(
                row()
                    .gap(10.0)
                    .center_y()
                    .child(
                        Button::new(if busy {
                            tr(language, "Connecting…", "连接中…")
                        } else {
                            tr(language, "Verify & Connect", "验证并连接")
                        })
                        .variant(ButtonVariant::Submit)
                        .build(k)
                        .key("pair-token-submit")
                        .on_press(move |s: &mut UiState| {
                            if token_url.is_empty() {
                                s.pairing_error = Some(
                                    tr(language, "Please provide a gateway URL", "请提供网关地址")
                                        .to_string(),
                                );
                                return;
                            }
                            if !busy && !token_val.is_empty() {
                                s.pairing_busy = true;
                                s.pairing_error = None;
                                s.send(Command::PairComputerToken {
                                    server_id: token_target.clone(),
                                    url: token_url.clone(),
                                    token: token_val.clone(),
                                });
                            }
                        }),
                    )
                    .child(
                        Button::new(tr(language, "Use Pairing Code instead", "切换为配对码关联"))
                            .variant(ButtonVariant::Ghost)
                            .build(k)
                            .key("pair-toggle-to-code")
                            .on_press(|s: &mut UiState| {
                                s.pairing_manual_token_mode = false;
                                s.pairing_error = None;
                            }),
                    )
                    .child(
                        Button::new(tr(language, "Cancel", "取消"))
                            .variant(ButtonVariant::Ghost)
                            .build(k)
                            .key("pair-token-cancel")
                            .on_press(|s: &mut UiState| s.close_pairing()),
                    ),
            );

        card = card.child(form);
    } else if let Some(req_id) = &s.pairing_request_id {
        // Mode A Stage 2: Code Entry with Cyber PIN Matrix
        let claim_target = target.clone();
        let claim_url = target_url.clone();
        let claim_req_id = req_id.clone();
        let transport_req = s.pairing_transport_required;

        let clean_code =
            crate::platform::pairing::normalize_pairing_code(&s.pairing_code_input.text);
        let chars: Vec<char> = clean_code.chars().filter(|c| *c != '-').collect();
        let is_complete = chars.len() == 8;

        let pin_matrix = build_pin_matrix(&clean_code, busy, k);

        let status_hint = if is_complete {
            row()
                .gap(6.0)
                .center_y()
                .child(icon(&glyphs::DONE, 14.0).color(k.c.success.fg))
                .child(
                    text(tr(
                        language,
                        "Code complete! Click Confirm & Connect below.",
                        "配对码完整！请点击下方按钮完成加密授权。",
                    ))
                    .font(k.ty.caption())
                    .color(k.c.success.fg),
                )
        } else {
            row()
                .gap(6.0)
                .center_y()
                .child(icon(&glyphs::TIME, 13.0).color(k.c.text_muted))
                .child(
                    text(format!(
                        "{}/8 · {}",
                        chars.len(),
                        tr(
                            language,
                            "Hyphen is handled automatically. Case-insensitive.",
                            "连字符自动处理，不区分大小写",
                        )
                    ))
                    .font(k.ty.caption())
                    .color(k.c.text_muted),
                )
        };

        let security_pill = row()
            .w_full()
            .gap(8.0)
            .center_y()
            .px(12.0)
            .py(8.0)
            .radius(8.0)
            .fill(k.c.layer_01.alpha(0.5))
            .border(1.0, k.c.border_weak)
            .child(icon(&glyphs::TRUST_WARNING, 14.0).color(k.c.text_muted))
            .child(
                text(tr(
                    language,
                    "Zero-Trust Handshake: Key derived locally via Argon2id (ChaCha20-Poly1305).",
                    "零信任加密握手：通过 Argon2id 本地派生端到端传输密钥。",
                ))
                .font(k.ty.caption())
                .color(k.c.text_muted),
            );

        let form = col()
            .gap(12.0)
            .child(
                row()
                    .gap(8.0)
                    .center_y()
                    .child(
                        row()
                            .px(7.0)
                            .py(2.0)
                            .radius(4.0)
                            .fill(k.c.bg_contrast)
                            .child(
                                text(tr(language, "CODE", "配对码"))
                                    .font(TextStyle::new(10.0, 800.0).mono())
                                    .color(k.c.text_contrast),
                            ),
                    )
                    .child(
                        text(tr(
                            language,
                            "Enter the 8-character code shown on the remote screen:",
                            "输入远端屏幕显示的 8 位配对码：",
                        ))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base),
                    ),
            )
            .child(pin_matrix)
            .child(
                TextInput::new(&s.pairing_code_input)
                    .placeholder("VDKQ-7QUF")
                    .label(super::copy::tr_ref(language, "Direct Input / Paste", "键盘输入或粘贴"))
                    .text_style(TextStyle::new(15.0, 600.0).mono().lh(20.0))
                    .height(38.0)
                    .width(None)
                    .on_edit(|s: &mut UiState| &mut s.pairing_code_input)
                    .on_change(|s: &mut UiState, _: &mut Tx| {
                        let normalized = crate::platform::pairing::normalize_pairing_code(
                            &s.pairing_code_input.text,
                        );
                        if normalized != s.pairing_code_input.text {
                            s.pairing_code_input.text = normalized;
                        }
                    })
                    .build(k)
                    .key("pair-code-input"),
            )
            .child(status_hint)
            .child(security_pill)
            .child(
                row()
                    .gap(10.0)
                    .center_y()
                    .child(
                        Button::new(if busy {
                            tr(language, "Claiming…", "正在验证…")
                        } else {
                            tr(language, "Confirm & Connect", "确认并连接")
                        })
                        .variant(ButtonVariant::Submit)
                        .build(k)
                        .key("pair-code-submit")
                        .on_press(move |s: &mut UiState| {
                            let final_code = crate::platform::pairing::normalize_pairing_code(
                                &s.pairing_code_input.text,
                            );
                            if !busy && final_code.len() == 9 {
                                s.pairing_busy = true;
                                s.pairing_error = None;
                                s.send(Command::PairComputerClaim {
                                    server_id: claim_target.clone(),
                                    url: claim_url.clone(),
                                    request_id: claim_req_id.clone(),
                                    code: final_code,
                                    transport_required: transport_req,
                                });
                            } else if final_code.len() < 9 {
                                s.pairing_error = Some(
                                    tr(
                                        language,
                                        "Please enter the complete 8-character code with hyphen (e.g. VDKQ-7QUF)",
                                        "请输入完整的 8 位配对码（如 VDKQ-7QUF）",
                                    )
                                    .to_string(),
                                );
                            }
                        }),
                    )
                    .child(
                        Button::new(tr(language, "Cancel", "取消"))
                            .variant(ButtonVariant::Ghost)
                            .build(k)
                            .key("pair-code-cancel")
                            .on_press(|s: &mut UiState| {
                                s.pairing_request_id = None;
                                s.pairing_busy = false;
                                s.pairing_error = None;
                            }),
                    ),
            );

        card = card.child(form);
    } else {
        // Mode A Stage 1: Handshake Request Hero
        let req_target = target.clone();
        let req_url = target_url.clone();

        let handshake_hero = col()
            .w_full()
            .gap(14.0)
            .px(20.0)
            .py(20.0)
            .radius(14.0)
            .fill(k.c.layer_01.alpha(0.6))
            .border(1.0, k.c.border_weak)
            .child(
                row()
                    .w_full()
                    .center_y()
                    .justify_center()
                    .gap(16.0)
                    .child(
                        col()
                            .items_center()
                            .gap(6.0)
                            .child(
                                col()
                                    .w(44.0)
                                    .h(44.0)
                                    .radius(10.0)
                                    .fill(k.c.surface_base)
                                    .border(1.0, k.c.border_base)
                                    .items_center()
                                    .justify_center()
                                    .child(icon(&glyphs::COMPUTER, 22.0).color(k.c.text_base)),
                            )
                            .child(
                                text(tr(language, "Desktop", "桌面端"))
                                    .font(k.ty.caption())
                                    .color(k.c.text_muted),
                            ),
                    )
                    .child(
                        col()
                            .items_center()
                            .gap(4.0)
                            .child(
                                text("──────⚡──────")
                                    .font(TextStyle::new(12.0, 700.0).mono())
                                    .color(k.c.border_focus),
                            )
                            .child(
                                text(tr(language, "P2P Link", "直连信道"))
                                    .font(k.ty.caption())
                                    .color(k.c.border_focus),
                            ),
                    )
                    .child(
                        col()
                            .items_center()
                            .gap(6.0)
                            .child(
                                col()
                                    .w(44.0)
                                    .h(44.0)
                                    .radius(10.0)
                                    .fill(k.c.surface_base)
                                    .border(1.0, k.c.border_base)
                                    .items_center()
                                    .justify_center()
                                    .child(icon(&glyphs::TERMINAL, 22.0).color(k.c.text_base)),
                            )
                            .child(
                                text(name.clone())
                                    .font(k.ty.caption())
                                    .color(k.c.text_muted),
                            ),
                    ),
            )
            .child(
                text(tr(
                    language,
                    "Link this desktop to browse and run sessions on this remote computer. Click below to request a one-time pairing code from the remote terminal.",
                    "完成一次性配对授权，即可在本地桌面端直接浏览、创建和管理远端会话。点击下方按钮即可向远端网关请求配对码并在屏幕上查看。",
                ))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .w_full(),
            );

        let form = col().gap(14.0).child(handshake_hero).child(
            row()
                .gap(10.0)
                .center_y()
                .child(
                    Button::new(if busy {
                        tr(language, "Requesting…", "请求中…")
                    } else {
                        tr(language, "Request Pairing Code", "发起配对请求")
                    })
                    .variant(ButtonVariant::Submit)
                    .build(k)
                    .key("pair-request-btn")
                    .on_press(move |s: &mut UiState| {
                        if req_url.is_empty() {
                            s.pairing_error = Some(
                                tr(language, "Please provide a gateway URL", "请提供网关地址")
                                    .to_string(),
                            );
                            return;
                        }
                        if !busy {
                            s.pairing_busy = true;
                            s.pairing_error = None;
                            s.send(Command::PairComputerRequest {
                                server_id: req_target.clone(),
                                url: req_url.clone(),
                            });
                        }
                    }),
                )
                .child(
                    Button::new(tr(language, "Enter Token directly", "直接输入 Token"))
                        .variant(ButtonVariant::Ghost)
                        .build(k)
                        .key("pair-toggle-to-token")
                        .on_press(|s: &mut UiState| {
                            s.pairing_manual_token_mode = true;
                            s.pairing_error = None;
                        }),
                )
                .child(
                    Button::new(tr(language, "Cancel", "取消"))
                        .variant(ButtonVariant::Ghost)
                        .build(k)
                        .key("pair-request-cancel")
                        .on_press(|s: &mut UiState| s.close_pairing()),
                ),
        );

        card = card.child(form);
    }

    let mut panel = look
        .panel(k, card, crate::theme::look::PANEL_RADIUS)
        .fill(k.c.surface_raised_base.alpha(0.97));
    for shadow in k.shadow_layers(&k.elevation.overlay) {
        panel = panel.box_shadow(shadow);
    }

    let panel = panel.modal(true, |s: &mut UiState| s.close_pairing());

    // Scrim overlay
    layer.push(
        rune::el()
            .key("pair-scrim")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .fill(k.c.black.alpha(if s.dark { 0.5 } else { 0.26 }))
            .token_motion(k.motion.overlay)
            .a11y_hidden(),
    );

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

fn build_pin_matrix(clean_code: &str, busy: bool, k: &Kit) -> El {
    let chars: Vec<char> = clean_code.chars().filter(|c| *c != '-').collect();
    let is_complete = chars.len() == 8;

    let mut row_el = row()
        .key("pin-matrix")
        .w_full()
        .center_y()
        .justify_center()
        .gap(8.0);

    // Left group: slots 0..4
    for i in 0..4 {
        row_el = row_el.child(render_pin_cell(i, &chars, busy, is_complete, k));
    }

    // Divider
    row_el = row_el.child(
        col()
            .key("pin-divider")
            .w(18.0)
            .h(58.0)
            .items_center()
            .justify_center()
            .child(
                text("—")
                    .font(TextStyle::new(18.0, 700.0).mono())
                    .color(k.c.text_muted),
            ),
    );

    // Right group: slots 4..8
    for i in 4..8 {
        row_el = row_el.child(render_pin_cell(i, &chars, busy, is_complete, k));
    }

    let spaced = crate::platform::pairing::format_spaced_code(clean_code);

    col()
        .key("pin-matrix-container")
        .role("group")
        .label(if spaced.is_empty() {
            "Pairing code".to_string()
        } else {
            spaced
        })
        .w_full()
        .py(16.0)
        .px(12.0)
        .radius(12.0)
        .fill(k.c.surface_base.alpha(0.5))
        .border(
            1.5,
            if is_complete {
                k.c.border_focus
            } else {
                k.c.border_base
            },
        )
        .items_center()
        .justify_center()
        .on_press(|s: &mut UiState| {
            s.pairing_code_input.focused = true;
        })
        .child(row_el)
}

fn render_pin_cell(index: usize, chars: &[char], busy: bool, is_complete: bool, k: &Kit) -> El {
    let is_active = index == chars.len() && !busy && !is_complete;
    let char_opt = chars.get(index).copied();

    let mut cell = col()
        .key(format!("pin-cell-{index}"))
        .w(44.0)
        .h(56.0)
        .radius(10.0)
        .items_center()
        .justify_center();

    if let Some(ch) = char_opt {
        cell = cell
            .fill(k.c.layer_01)
            .border(
                1.5,
                if is_complete {
                    k.c.border_focus
                } else {
                    k.c.border_base
                },
            )
            .box_shadow(rune::Shadow {
                x: 0.0,
                y: 1.5,
                blur: 3.0,
                spread: 0.0,
                color: k.c.border_base.alpha(0.2),
                inset: false,
            })
            .child(
                text(ch.to_string())
                    .font(TextStyle::new(26.0, 800.0).mono().lh(32.0))
                    .color(k.c.text_base),
            );
    } else if is_active {
        cell = cell
            .fill(k.c.layer_01)
            .border(2.0, k.c.border_focus)
            .box_shadow(rune::Shadow {
                x: 0.0,
                y: 0.0,
                blur: 8.0,
                spread: 1.0,
                color: k.c.border_focus.alpha(0.45),
                inset: false,
            })
            .child(
                text("·")
                    .font(TextStyle::new(28.0, 900.0).mono().lh(32.0))
                    .color(k.c.border_focus),
            );
    } else {
        cell = cell
            .fill(k.c.surface_base.alpha(0.35))
            .border(1.0, k.c.border_weak)
            .child(
                text("·")
                    .font(TextStyle::new(20.0, 700.0).mono().lh(24.0))
                    .color(k.c.text_faint),
            );
    }

    cell
}
