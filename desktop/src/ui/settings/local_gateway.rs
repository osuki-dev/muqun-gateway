//! Settings → Computers → This computer's Gateway:
//! graphical management for the local Gateway's identity, network address/port,
//! transport security, remote/push flags, login service, and terminal backends.

use muqun_gateway::gateway_control::{
    GatewayControlAction, GatewayControlOutcome, GatewayControlSnapshot, ServiceState,
};
use rune::{col, el, row as hrow, text, El, Key, TextEdit, Tx};
use rune_kit::{ButtonVariant, Kit, TextInput};

use super::{button, field, flag, group, group_action, row};
use crate::model::commands::Command;
use crate::ui::copy::{tr, tr_ref};
use crate::ui::UiState;

#[derive(Default)]
pub(crate) struct LocalGatewayPanel {
    pub loading: bool,
    pub busy: bool,
    pub snapshot: Option<GatewayControlSnapshot>,
    pub error: Option<String>,
    pub label_input: TextEdit,
    pub public_url_input: TextEdit,
    pub port_input: TextEdit,
}

impl LocalGatewayPanel {
    fn sync_inputs(&mut self, snap: &GatewayControlSnapshot) {
        self.label_input = TextEdit::new(snap.label.clone());
        self.public_url_input = TextEdit::new(snap.public_url.clone());
        self.port_input = TextEdit::new(snap.port.to_string());
    }
}

impl UiState {
    pub fn refresh_local_gateway_control(&mut self) {
        if self.local_gateway.loading || self.local_gateway.busy {
            return;
        }
        self.local_gateway.loading = true;
        self.send(Command::RefreshLocalGatewayControl);
    }

    pub(crate) fn run_local_gateway_action(&mut self, action: GatewayControlAction) {
        if self.local_gateway.busy {
            return;
        }
        self.local_gateway.busy = true;
        self.local_gateway.error = None;
        self.send(Command::RunLocalGatewayControl(action));
    }

    pub(crate) fn receive_local_gateway_control(
        &mut self,
        result: Result<GatewayControlSnapshot, String>,
    ) {
        self.local_gateway.loading = false;
        match result {
            Ok(snap) => {
                self.local_gateway.sync_inputs(&snap);
                self.local_gateway.snapshot = Some(snap);
                self.local_gateway.error = None;
            }
            Err(err) => {
                self.local_gateway.error = Some(err);
            }
        }
    }

    pub(crate) fn finish_local_gateway_action(
        &mut self,
        result: Result<GatewayControlOutcome, String>,
    ) {
        self.local_gateway.busy = false;
        let lang = self.cfg.general.language;
        match result {
            Ok(outcome) => {
                self.local_gateway.sync_inputs(&outcome.snapshot);
                self.local_gateway.snapshot = Some(outcome.snapshot);
                self.local_gateway.error = None;
                let detail = outcome
                    .restart_recommended
                    .then(|| tr(lang, "Restart Gateway to apply changes."));
                self.toast(outcome.message, detail);
                self.refresh_local_pairings();
            }
            Err(err) => {
                self.local_gateway.error = Some(err.clone());
                self.toast_error(tr_ref(lang, "Gateway setting failed"), Some(err));
            }
        }
    }
}

fn status_badge(k: &Kit, label: &str, color: rune::Color) -> El {
    hrow()
        .gap(6.0)
        .px(8.0)
        .py(3.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(color.alpha(0.14))
        .border(1.0, color.alpha(0.35))
        .child(el().size(6.0, 6.0).radius(3.0).fill(color).a11y_hidden())
        .child(text(label.to_string()).font(k.ty.small_medium).color(color))
}

fn save_port(s: &mut UiState) {
    let raw = s.local_gateway.port_input.text.trim();
    match raw.parse::<u16>().ok().filter(|&p| p >= 1) {
        Some(port) => s.run_local_gateway_action(GatewayControlAction::SetPort(port)),
        None => {
            let lang = s.cfg.general.language;
            s.toast_error(
                tr_ref(lang, "Invalid port"),
                Some(tr(lang, "Enter a port number between 1 and 65535.")),
            );
        }
    }
}

pub(super) fn groups(s: &UiState, k: &Kit) -> Vec<El> {
    let lang = s.cfg.general.language;
    let tr_s = |key| tr_ref(lang, key);
    let panel = &s.local_gateway;
    let busy = panel.busy || panel.loading;

    let Some(snap) = &panel.snapshot else {
        let msg = if panel.loading {
            tr_s("Loading…")
        } else if let Some(err) = &panel.error {
            err.as_str()
        } else {
            tr_s("Local Gateway unavailable")
        };
        let actions = button(tr_s("Refresh"))
            .disabled(busy)
            .on_press(UiState::refresh_local_gateway_control)
            .build(k)
            .key("local-gateway-refresh");
        return vec![group_action(
            k,
            "local-gateway-control",
            tr_s("This computer's Gateway"),
            actions,
            vec![text(msg.to_string()).font(k.ty.ui).color(k.c.text_muted)],
        )];
    };

    let mut header_actions = hrow().gap(8.0).flex_wrap().center_y().child(
        button(tr_s("Refresh"))
            .disabled(busy)
            .on_press(UiState::refresh_local_gateway_control)
            .build(k)
            .key("local-gateway-refresh"),
    );
    if snap.configured && snap.cli_available {
        if snap.running {
            header_actions = header_actions
                .child(
                    button(tr_s("Restart"))
                        .variant(if snap.config_changed_since_start {
                            ButtonVariant::Submit
                        } else {
                            ButtonVariant::Neutral
                        })
                        .disabled(busy)
                        .on_press(|s: &mut UiState| {
                            s.run_local_gateway_action(GatewayControlAction::RestartGateway);
                        })
                        .build(k)
                        .key("local-gateway-restart"),
                )
                .child(
                    button(tr_s("Stop"))
                        .variant(ButtonVariant::Danger)
                        .disabled(busy)
                        .on_press(|s: &mut UiState| {
                            s.run_local_gateway_action(GatewayControlAction::StopGateway);
                        })
                        .build(k)
                        .key("local-gateway-stop"),
                );
        } else {
            header_actions = header_actions.child(
                button(tr_s("Start"))
                    .variant(ButtonVariant::Submit)
                    .disabled(busy)
                    .on_press(|s: &mut UiState| {
                        s.run_local_gateway_action(GatewayControlAction::StartGateway);
                    })
                    .build(k)
                    .key("local-gateway-start"),
            );
        }
    }

    if !snap.configured {
        return vec![group_action(
            k,
            "local-gateway-control",
            tr_s("This computer's Gateway"),
            header_actions,
            vec![text(tr(
                lang,
                "Local Gateway is not configured yet. Run `muqun-gateway setup` to initialize it.",
            ))
            .font(k.ty.ui)
            .color(k.c.text_muted)
            .wrap()],
        )];
    }

    let status_text = if snap.running {
        match snap.pid {
            Some(pid) => format!("{} (PID {pid})", tr_s("Running")),
            None => tr(lang, "Running"),
        }
    } else {
        tr(lang, "Stopped")
    };
    let status_color = if snap.running {
        k.c.success.icon
    } else {
        k.c.warning.icon
    };
    let status_detail = if snap.config_changed_since_start {
        tr(lang, "Config changed on disk — restart Gateway to apply.")
    } else {
        format!("{} · {}", snap.listen, snap.config_path)
    };

    let mut main_rows = vec![row(
        k,
        "gw-status",
        tr_s("Gateway status"),
        &status_detail,
        status_badge(k, &status_text, status_color),
    )];

    let login_installed = matches!(
        snap.service_state,
        ServiceState::Installed | ServiceState::Stopped
    );
    let login_help = match snap.service_state {
        ServiceState::Installed => tr_s("Registered to start automatically when you sign in."),
        ServiceState::Stopped => tr_s("Registered for sign-in autostart (currently stopped)."),
        ServiceState::FileOnly => tr_s("Service file exists on disk but is not enabled."),
        ServiceState::NotInstalled => {
            tr_s("Register with systemd / LaunchAgent so Gateway starts at login.")
        }
    };
    if snap.cli_available {
        main_rows.push(row(
            k,
            "gw-autostart",
            tr_s("Start at login"),
            login_help,
            flag(
                k,
                "gw-autostart",
                login_installed,
                tr_s("Start at login"),
                move |s: &mut UiState| {
                    s.run_local_gateway_action(if login_installed {
                        GatewayControlAction::UninstallService
                    } else {
                        GatewayControlAction::InstallService
                    });
                },
            ),
        ));
    }

    let label_dirty = panel.label_input.text.trim() != snap.label;
    main_rows.push(row(
        k,
        "gw-label",
        tr_s("Computer name"),
        tr_s("Shown on paired phones and other computers."),
        hrow()
            .gap(8.0)
            .center_y()
            .child(field(
                TextInput::new(&panel.label_input)
                    .label(tr_s("Computer name"))
                    .placeholder("workstation")
                    .on_edit(|s: &mut UiState| &mut s.local_gateway.label_input)
                    .on_key(|s: &mut UiState, ev: &rune::KeyEvent, _: &mut Tx| {
                        if ev.key == Key::Enter && !ev.mods.shift {
                            let val = s.local_gateway.label_input.text.clone();
                            s.run_local_gateway_action(GatewayControlAction::SetLabel(val));
                            return true;
                        }
                        false
                    }),
                &panel.label_input,
                k,
                "gw-label-input",
            ))
            .child(
                button(tr_s("Save"))
                    .variant(if label_dirty {
                        ButtonVariant::Submit
                    } else {
                        ButtonVariant::Neutral
                    })
                    .disabled(busy || !label_dirty)
                    .on_press(|s: &mut UiState| {
                        let val = s.local_gateway.label_input.text.clone();
                        s.run_local_gateway_action(GatewayControlAction::SetLabel(val));
                    })
                    .build(k)
                    .key("gw-label-save"),
            ),
    ));

    let url_dirty = panel.public_url_input.text.trim() != snap.public_url;
    main_rows.push(row(
        k,
        "gw-public-url",
        tr_s("Public address"),
        tr_s("Reachable LAN or Tailscale URL embedded in pairing QR codes."),
        col()
            .gap(8.0)
            .w_full()
            .child(field(
                TextInput::new(&panel.public_url_input)
                    .label(tr_s("Public address"))
                    .placeholder("http://100.x.y.z:23847")
                    .on_edit(|s: &mut UiState| &mut s.local_gateway.public_url_input)
                    .on_key(|s: &mut UiState, ev: &rune::KeyEvent, _: &mut Tx| {
                        if ev.key == Key::Enter && !ev.mods.shift {
                            let val = s.local_gateway.public_url_input.text.clone();
                            s.run_local_gateway_action(GatewayControlAction::SetPublicUrl(val));
                            return true;
                        }
                        false
                    }),
                &panel.public_url_input,
                k,
                "gw-public-url-input",
            ))
            .child(
                hrow()
                    .gap(8.0)
                    .flex_wrap()
                    .child(
                        button(tr_s("Save"))
                            .variant(if url_dirty {
                                ButtonVariant::Submit
                            } else {
                                ButtonVariant::Neutral
                            })
                            .disabled(busy || !url_dirty)
                            .on_press(|s: &mut UiState| {
                                let val = s.local_gateway.public_url_input.text.clone();
                                s.run_local_gateway_action(GatewayControlAction::SetPublicUrl(val));
                            })
                            .build(k)
                            .key("gw-public-url-save"),
                    )
                    .child(
                        button(tr_s("Detect address"))
                            .disabled(busy)
                            .on_press(|s: &mut UiState| {
                                s.run_local_gateway_action(
                                    GatewayControlAction::AutoDetectPublicUrl,
                                );
                            })
                            .build(k)
                            .key("gw-public-url-detect"),
                    ),
            ),
    ));

    let port_dirty = panel.port_input.text.trim() != snap.port.to_string();
    main_rows.push(row(
        k,
        "gw-port",
        tr_s("Listen port"),
        tr_s("TCP port this Gateway listens on (default 23847)."),
        hrow()
            .gap(8.0)
            .center_y()
            .child(
                TextInput::new(&panel.port_input)
                    .label(tr_s("Listen port"))
                    .placeholder("23847")
                    .width(Some(110.0))
                    .on_edit(|s: &mut UiState| &mut s.local_gateway.port_input)
                    .on_key(|s: &mut UiState, ev: &rune::KeyEvent, _: &mut Tx| {
                        if ev.key == Key::Enter && !ev.mods.shift {
                            save_port(s);
                            return true;
                        }
                        false
                    })
                    .build(k)
                    .key("gw-port-input"),
            )
            .child(
                button(tr_s("Save"))
                    .variant(if port_dirty {
                        ButtonVariant::Submit
                    } else {
                        ButtonVariant::Neutral
                    })
                    .disabled(busy || !port_dirty)
                    .on_press(save_port)
                    .build(k)
                    .key("gw-port-save"),
            ),
    ));

    let enc = snap.transport_encryption;
    let remote_auto = snap.allow_remote_auto_approve;
    let trust_folders = snap.agent_trust_folders;
    let rich_pushes = snap.rich_agent_pushes;

    let security_rows = vec![
        row(
            k,
            "gw-encryption",
            tr_s("Transport encryption"),
            tr_s("Encrypt HTTP requests and responses for newly paired devices."),
            flag(
                k,
                "gw-encryption",
                enc,
                tr_s("Transport encryption"),
                move |s: &mut UiState| {
                    s.run_local_gateway_action(GatewayControlAction::SetTransportEncryption(!enc));
                },
            ),
        ),
        row(
            k,
            "gw-remote-auto-approve",
            tr_s("Allow remote auto-approve"),
            tr_s("Let paired phones enable agent auto-approve on this computer."),
            flag(
                k,
                "gw-remote-auto-approve",
                remote_auto,
                tr_s("Allow remote auto-approve"),
                move |s: &mut UiState| {
                    s.run_local_gateway_action(GatewayControlAction::SetAllowRemoteAutoApprove(
                        !remote_auto,
                    ));
                },
            ),
        ),
        row(
            k,
            "gw-trust-folders",
            tr_s("Trust workspace folders"),
            tr_s("Automatically accept folder trust prompts for agents launched inside your home directory."),
            flag(
                k,
                "gw-trust-folders",
                trust_folders,
                tr_s("Trust workspace folders"),
                move |s: &mut UiState| {
                    s.run_local_gateway_action(GatewayControlAction::SetAgentTrustFolders(
                        !trust_folders,
                    ));
                },
            ),
        ),
        row(
            k,
            "gw-rich-pushes",
            tr_s("Detailed push notifications"),
            tr_s("Include agent questions and command details in mobile push notifications."),
            flag(
                k,
                "gw-rich-pushes",
                rich_pushes,
                tr_s("Detailed push notifications"),
                move |s: &mut UiState| {
                    s.run_local_gateway_action(GatewayControlAction::SetRichAgentPushes(
                        !rich_pushes,
                    ));
                },
            ),
        ),
    ];

    let has_tmux = snap.backends.iter().any(|b| b.kind == "tmux");
    let has_herdr = snap.backends.iter().any(|b| b.kind == "herdr");
    let mut backend_actions = hrow().gap(8.0).flex_wrap();
    if !has_tmux {
        backend_actions = backend_actions.child(
            button(tr_s("Add tmux"))
                .disabled(busy)
                .on_press(|s: &mut UiState| {
                    s.run_local_gateway_action(GatewayControlAction::AddBackend("tmux".into()));
                })
                .build(k)
                .key("gw-backend-add-tmux"),
        );
    }
    if !has_herdr {
        backend_actions = backend_actions.child(
            button(tr_s("Add Herdr"))
                .disabled(busy)
                .on_press(|s: &mut UiState| {
                    s.run_local_gateway_action(GatewayControlAction::AddBackend("herdr".into()));
                })
                .build(k)
                .key("gw-backend-add-herdr"),
        );
    }

    let can_remove = snap.backends.len() > 1;
    let backend_rows: Vec<El> = if snap.backends.is_empty() {
        vec![text(tr_s("No terminal backends configured"))
            .font(k.ty.ui)
            .color(k.c.text_muted)]
    } else {
        snap.backends
            .iter()
            .map(|b| {
                let id_def = b.id.clone();
                let id_auto = b.id.clone();
                let id_rm = b.id.clone();
                let autostart = b.autostart;
                let mut controls = hrow().gap(8.0).flex_wrap().center_y();
                if b.is_default {
                    controls = controls.child(status_badge(k, tr_s("Default"), k.c.bg_accent));
                } else {
                    controls = controls.child(
                        button(tr_s("Set default"))
                            .disabled(busy)
                            .on_press(move |s: &mut UiState| {
                                s.run_local_gateway_action(
                                    GatewayControlAction::SetDefaultBackend(id_def.clone()),
                                );
                            })
                            .build(k),
                    );
                }
                controls = controls.child(
                    hrow()
                        .gap(6.0)
                        .center_y()
                        .child(
                            text(tr_s("Autostart"))
                                .font(k.ty.small)
                                .color(k.c.text_muted),
                        )
                        .child(flag(
                            k,
                            &format!("gw-backend-auto-{}", b.id),
                            autostart,
                            tr_s("Autostart"),
                            move |s: &mut UiState| {
                                s.run_local_gateway_action(
                                    GatewayControlAction::SetBackendAutostart {
                                        id: id_auto.clone(),
                                        enabled: !autostart,
                                    },
                                );
                            },
                        )),
                );
                if can_remove {
                    controls = controls.child(
                        button(tr_s("Remove"))
                            .variant(ButtonVariant::Danger)
                            .disabled(busy)
                            .on_press(move |s: &mut UiState| {
                                s.run_local_gateway_action(GatewayControlAction::RemoveBackend(
                                    id_rm.clone(),
                                ));
                            })
                            .build(k),
                    );
                }
                let detail = format!("{} · {}", b.kind, b.endpoint);
                row(
                    k,
                    &format!("gw-backend-{}", b.id),
                    &b.label,
                    &detail,
                    controls,
                )
            })
            .collect()
    };

    vec![
        group_action(
            k,
            "local-gateway-control",
            tr_s("This computer's Gateway"),
            header_actions,
            main_rows,
        ),
        group(
            k,
            "local-gateway-security",
            tr_s("Gateway security & notifications"),
            security_rows,
        ),
        group_action(
            k,
            "local-gateway-backends",
            tr_s("Terminal backends"),
            backend_actions,
            backend_rows,
        ),
    ]
}
