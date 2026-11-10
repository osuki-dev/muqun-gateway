//! Computer Pairing Modal: shown when the user chooses an unpaired remote computer.

use rune::{col, row, text, El};
use rune_kit::{Button, ButtonVariant, Kit, TextInput};

use super::copy::tr;
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
        .w(520.0)
        .max_w(520.0)
        .gap(20.0)
        .px(32.0)
        .py(32.0);

    // 1. Header with Title, Status & Close Button
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

    let header = col()
        .gap(8.0)
        .child(
            row()
                .w_full()
                .center_y()
                .child(
                    row()
                        .gap(10.0)
                        .center_y()
                        .child(
                            text(format!("💻 {}", name))
                                .font(k.ty.ui_medium)
                                .color(k.c.text_base),
                        )
                        .child(
                            row()
                                .gap(6.0)
                                .center_y()
                                .px(8.0)
                                .py(2.0)
                                .radius(999.0)
                                .fill(k.c.layer_01)
                                .border(1.0, k.c.border_muted)
                                .child(rune::el().w(6.0).h(6.0).radius(3.0).fill(status_color))
                                .child(
                                    text(status_text).font(k.ty.caption()).color(k.c.text_muted),
                                ),
                        ),
                )
                .child(rune::el().grow(1.0))
                .child(
                    Button::new("✕")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut UiState| s.close_pairing())
                        .build(k)
                        .key("pair-close-x"),
                ),
        )
        .child(
            text(if effective_url.is_empty() {
                tr(
                    language,
                    "Linked via fleet gateway",
                    "已通过网关舰队对等关联",
                )
            } else {
                effective_url.clone()
            })
            .font(k.ty.caption())
            .color(k.c.text_muted),
        );

    card = card.child(header);

    // 2. Explanation
    card = card.child(
        text(tr(
            language,
            "This computer is linked for remote commands. To browse, create, and manage sessions on this machine directly, link this desktop with a one-time authorization.",
            "此电脑已通过网关对等互联（可用于远程命令与任务调度）。若要在本桌面端直接查看、新建和管理会话，请在此完成一次关联授权。",
        ))
        .font(k.ty.ui)
        .color(k.c.text_muted)
        .wrap()
        .w_full(),
    );

    // 3. If URL is missing, offer input
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

    // 4. Error message if any
    if let Some(err) = &s.pairing_error {
        card = card.child(
            row()
                .w_full()
                .gap(8.0)
                .px(12.0)
                .py(10.0)
                .radius(8.0)
                .fill(k.c.layer_01)
                .border(1.0, k.c.danger.border)
                .child(
                    text(format!("⚠️ {}", err))
                        .font(k.ty.small)
                        .color(k.c.danger.fg)
                        .wrap()
                        .w_full(),
                ),
        );
    }

    // 5. Pairing Form: Mode A (Pairing Code) vs Mode B (Manual Token)
    let busy = s.pairing_busy;
    let target = target_id.clone();
    let target_url = effective_url.clone();

    if s.pairing_manual_token_mode {
        // Mode B: Manual Token
        let token_target = target.clone();
        let token_url = target_url.clone();
        let token_val = s.pairing_token_input.text.trim().to_string();

        let form = col()
            .gap(12.0)
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
    } else {
        // Mode A: Pairing Code
        if let Some(req_id) = &s.pairing_request_id {
            let claim_target = target.clone();
            let claim_url = target_url.clone();
            let claim_req_id = req_id.clone();
            let claim_code = s.pairing_code_input.text.trim().to_string();
            let transport_req = s.pairing_transport_required;

            let form = col()
                .gap(12.0)
                .child(
                    text(format!(
                        "{}:",
                        tr(
                            language,
                            "Pairing code requested! Please enter the code shown on the remote machine",
                            "已发起配对请求！请输入远端机器上显示的配对码",
                        )
                    ))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .wrap()
                    .w_full(),
                )
                .child(
                    TextInput::new(&s.pairing_code_input)
                        .placeholder("ABCD-1234")
                        .on_edit(|s: &mut UiState| &mut s.pairing_code_input)
                        .build(k)
                        .key("pair-code-input"),
                )
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
                                if !busy && !claim_code.is_empty() {
                                    s.pairing_busy = true;
                                    s.pairing_error = None;
                                    s.send(Command::PairComputerClaim {
                                        server_id: claim_target.clone(),
                                        url: claim_url.clone(),
                                        request_id: claim_req_id.clone(),
                                        code: claim_code.clone(),
                                        transport_required: transport_req,
                                    });
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
            let req_target = target.clone();
            let req_url = target_url.clone();

            let form = col().gap(12.0).child(
                row()
                    .gap(10.0)
                    .center_y()
                    .child(
                        Button::new(if busy {
                            tr(language, "Requesting…", "请求中…")
                        } else {
                            tr(language, "Request Pairing Code", "获取配对码关联")
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
