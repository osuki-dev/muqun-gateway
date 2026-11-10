//! Computer Pairing View: shown when a selected remote computer is not yet paired.

use rune::{col, row, text, El};
use rune_kit::{Button, ButtonVariant, Kit, TextInput};

use super::copy::tr;
use super::UiState;
use crate::model::commands::Command;

pub fn view(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let curr = s.current_computer().to_string();
    let name = s.computer_name(&curr);
    let peer = s.peer_by_id(&curr);
    let url = s.peer_url(&curr).unwrap_or_default();
    let reachable = peer.map(|p| p.reachable).unwrap_or(true);

    let mut card = col()
        .key("pair-computer-card")
        .w(520.0)
        .max_w(520.0)
        .gap(20.0)
        .px(32.0)
        .py(32.0)
        .radius(16.0)
        .fill(k.c.surface_raised_base)
        .border(1.0, k.c.border_base);

    // 1. Header with Title & Computer Badge
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
                        .child(text(status_text).font(k.ty.caption()).color(k.c.text_muted)),
                ),
        )
        .child(
            text(if url.is_empty() {
                tr(
                    language,
                    "Linked via fleet gateway",
                    "已通过网关舰队对等关联",
                )
            } else {
                url.clone()
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
        .color(k.c.text_muted),
    );

    // 3. Error message if any
    if let Some(err) = &s.pairing_error {
        card = card.child(
            row()
                .gap(8.0)
                .px(12.0)
                .py(10.0)
                .radius(8.0)
                .fill(k.c.layer_01)
                .border(1.0, k.c.danger.border)
                .child(
                    text(format!("⚠️ {}", err))
                        .font(k.ty.small)
                        .color(k.c.danger.fg),
                ),
        );
    }

    // 4. Pairing Form: Mode A (Pairing Code) vs Mode B (Manual Token)
    let busy = s.pairing_busy;
    let target_id = curr.clone();
    let target_url = url.clone();

    if s.pairing_manual_token_mode {
        // Mode B: Manual Token
        let token_target = target_id.clone();
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
                    ),
            );

        card = card.child(form);
    } else {
        // Mode A: Pairing Code
        if let Some(req_id) = &s.pairing_request_id {
            let claim_target = target_id.clone();
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
                    .color(k.c.text_base),
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
            let req_target = target_id.clone();
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
                    ),
            );

            card = card.child(form);
        }
    }

    col()
        .key("pair-computer-container")
        .w_full()
        .h_full()
        .center()
        .px(24.0)
        .py(24.0)
        .child(card)
}
