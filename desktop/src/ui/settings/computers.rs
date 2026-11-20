//! Settings → Computers. A saved login remains available when a peer is offline.
use crate::config::ConfigEdit;
use crate::model::commands::Command;
use rune::{text, El};
use rune_kit::{ButtonVariant, Kit};

use super::{button, group, row};
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

#[derive(Default)]
pub enum Unpair {
    #[default]
    Idle,
    Confirm(String),
    Pending(String, uuid::Uuid),
    Failed(String),
    ConfirmForget(String),
}

impl Unpair {
    pub fn dismiss(&mut self) {
        if !matches!(self, Self::Pending(..)) {
            *self = Self::Idle;
        }
    }

    pub fn invalidate(&mut self, id: &str) {
        if matches!(self, Self::Confirm(target) | Self::Pending(target, _) | Self::Failed(target) | Self::ConfirmForget(target) if target == id)
        {
            *self = Self::Idle;
        }
    }
}

impl UiState {
    fn request_computer_unpair(&mut self, id: &str) {
        if !matches!(&self.computer_unpair, Unpair::Confirm(target) if target == id) {
            return;
        }
        let Some(profile) = self.cfg.computers.get(id) else {
            return;
        };
        let Some((url, token)) = profile.url.clone().zip(profile.token.clone()) else {
            self.computer_unpair = Unpair::Failed(id.into());
            return;
        };
        let request = uuid::Uuid::new_v4();
        self.computer_unpair = Unpair::Pending(id.into(), request);
        self.send(Command::UnpairComputer {
            computer: id.into(),
            request,
            gateway: crate::hub::LocalGateway {
                url,
                token,
                device_id: profile.device_id.clone(),
                transport_key: profile.transport_key.clone(),
            },
        });
    }

    pub fn finish_computer_unpair(
        &mut self,
        id: String,
        request: uuid::Uuid,
        result: Result<(), String>,
    ) {
        if !matches!(&self.computer_unpair, Unpair::Pending(target, pending) if target == &id && *pending == request)
        {
            return;
        }
        match result {
            Ok(()) => self.forget_computer(&id),
            Err(err) => {
                self.computer_unpair = Unpair::Failed(id);
                self.toast_error(
                    tr_ref(self.cfg.general.language, "Could not unpair computer"),
                    Some(err),
                );
            }
        }
    }

    fn forget_computer(&mut self, id: &str) {
        if matches!(id, "" | "local" | "self" | "all") || !self.is_computer_paired(id) {
            return;
        }
        // Save the current view to its own cache before dropping that cache.
        if self.current_computer() == id {
            self.switch_computer("local".into());
        }
        self.forgotten_computers.insert(id.into());
        self.computer_data.remove(id);
        self.on_fleet_attention(crate::model::fleet_attention::Change {
            computer: id.into(),
            pending: vec![],
            raised: vec![],
            info: Default::default(),
        });
        self.fleet_pending.remove(id);
        if self.current_computer() == "all" {
            self.rebuild_all_sessions();
        }
        self.edit(ConfigEdit::ForgetComputer(id.into()));
        self.computer_unpair = Unpair::Idle;
    }
}

fn unpair_control(s: &UiState, k: &Kit, id: &str) -> El {
    let lang = s.cfg.general.language;
    let tr = |key| tr_ref(lang, key);
    let mut controls = rune::row().flex_wrap().center_y().gap(6.0);
    let id = id.to_owned();
    match &s.computer_unpair {
        Unpair::Confirm(target) if target == &id => {
            controls = controls.child(
                button(tr("Confirm unpair"))
                    .variant(ButtonVariant::Danger)
                    .on_press(move |s: &mut UiState| s.request_computer_unpair(&id))
                    .build(k),
            );
        }
        Unpair::ConfirmForget(target) if target == &id => {
            controls = controls.child(button(tr("Confirm forget")).variant(ButtonVariant::Danger)
                .on_press(move |s: &mut UiState| {
                    if matches!(&s.computer_unpair, Unpair::ConfirmForget(target) if target == &id) {s.forget_computer(&id);}
                }).build(k));
        }
        Unpair::Pending(target, _) => {
            return button(tr(if target == &id {
                "Unpairing…"
            } else {
                "Unpair"
            }))
            .disabled(true)
            .build(k);
        }
        state => {
            if matches!(state, Unpair::Failed(target) if target == &id) {
                let target = id.clone();
                controls = controls.child(
                    button(tr("Forget on this Desktop"))
                        .on_press(move |s: &mut UiState| {
                            if matches!(&s.computer_unpair, Unpair::Failed(id) if id == &target) {
                                s.computer_unpair = Unpair::ConfirmForget(target.clone());
                            }
                        })
                        .build(k),
                );
            }
            return controls.child(
                button(tr("Unpair"))
                    .on_press(move |s: &mut UiState| {
                        if !matches!(s.computer_unpair, Unpair::Pending(..)) {
                            s.computer_unpair = Unpair::Confirm(id.clone());
                        }
                    })
                    .build(k),
            );
        }
    }
    controls.child(
        button(tr("Cancel"))
            .on_press(|s: &mut UiState| s.computer_unpair.dismiss())
            .build(k),
    )
}

pub(super) fn actions(s: &UiState, k: &Kit) -> Vec<El> {
    vec![button(tr_ref(s.cfg.general.language, "Pair a computer"))
        .on_press(UiState::open_manual_pairing)
        .build(k)
        .key("computers-pair")]
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let lang = s.cfg.general.language;
    let mut paired = Vec::new();
    let mut discovered = Vec::new();
    let mut peers = s.all_computers();
    peers.sort_by(|a, b| (&a.name, &a.server_id).cmp(&(&b.name, &b.server_id)));
    for peer in peers {
        let id = peer.server_id;
        if matches!(id.as_str(), "local" | "self" | "all") {
            continue;
        }
        let saved = s.is_computer_paired(&id);
        let live = s.peers.iter().find(|p| p.server_id == id);
        if !saved && live.is_none() {
            continue;
        }
        let status = live.map(|p| tr_ref(lang, if p.reachable { "Online" } else { "Offline" }));
        let address = s.peer_url(&id);
        let detail = [status, address.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let key = format!("computer-{id}");
        let current = s.current_computer() == id && saved;
        let target_id = id.clone();
        let action = if current {
            text(tr_ref(lang, "Current"))
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
        } else if saved {
            button(tr_ref(lang, "Switch computer"))
                .on_press(move |s: &mut UiState| s.switch_computer(target_id.clone()))
                .build(k)
        } else {
            button(tr_ref(lang, "Pair"))
                .on_press(move |s: &mut UiState| s.open_pairing(target_id.clone()))
                .build(k)
        };
        let action = if saved {
            rune::row()
                .flex_wrap()
                .center_y()
                .gap(6.0)
                .child(action)
                .child(unpair_control(s, k, &id))
        } else {
            action
        };
        let rows = if saved { &mut paired } else { &mut discovered };
        rows.push(row(k, &key, &peer.name, &detail, action));
    }
    if paired.is_empty() {
        paired.push(
            text(tr_ref(lang, "No paired computers"))
                .font(k.ty.ui)
                .color(k.c.text_muted),
        );
    }
    let mut groups = vec![group(
        k,
        "paired-computers",
        tr_ref(lang, "Paired computers"),
        paired,
    )];
    if !discovered.is_empty() {
        groups.push(group(
            k,
            "discovered-computers",
            tr_ref(lang, "Discovered computers"),
            discovered,
        ));
    }
    groups.push(super::local_devices::group(s, k));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpair_handles_offline_fallback_and_late_completion_without_touching_other_logins() {
        let (commands, mut receiver) = crate::model::commands::channel();
        let mut s = UiState::new(commands);
        for id in ["remote", "other"] {
            s.cfg.computers.insert(
                id.into(),
                crate::config::ComputerProfile {
                    url: Some(format!("http://{id}")),
                    token: Some(id.into()),
                    device_id: Some(format!("device-{id}")),
                    transport_key: Some("key".into()),
                    theme_id: Some("saved-theme".into()),
                    ..Default::default()
                },
            );
        }
        s.cfg.current_computer = "remote".into();
        s.computer_unpair = Unpair::Confirm("remote".into());
        s.request_computer_unpair("remote");
        let mut unpairs = std::iter::from_fn(|| receiver.try_recv().ok()).filter_map(|command| {
            if let Command::UnpairComputer {
                computer,
                request,
                gateway,
            } = command
            {
                Some((computer, request, gateway))
            } else {
                None
            }
        });
        let (computer, request, gateway) = unpairs.next().expect("missing unpair request");
        assert!(unpairs.next().is_none(), "one command per confirmation");
        assert_eq!(computer, "remote");
        assert_eq!(gateway.token, "remote");
        s.finish_computer_unpair(computer.clone(), uuid::Uuid::new_v4(), Ok(()));
        assert!(s.is_computer_paired("remote"));
        s.finish_computer_unpair(computer, request, Err("offline".into()));
        assert!(s.is_computer_paired("remote"));
        assert!(matches!(s.computer_unpair, Unpair::Failed(_)));
        s.forget_computer("remote");
        let profile = &s.cfg.computers["remote"];
        assert!(
            profile.token.is_none()
                && profile.url.is_none()
                && profile.device_id.is_none()
                && profile.transport_key.is_none()
        );
        assert_eq!(profile.theme_id.as_deref(), Some("saved-theme"));
        assert_eq!(s.cfg.computers["other"].token.as_deref(), Some("other"));
        assert_eq!(s.current_computer(), "local");
        assert!(!s
            .all_computers()
            .iter()
            .any(|peer| peer.server_id == "remote"));
        assert!(!s.computer_data.contains_key("remote"));
    }

    #[test]
    fn computers_settings_opens_address_pairing_without_a_discovered_peer() {
        fn view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let mut layer = Vec::new();
            let settings = super::super::view(s, &Kit::neutral(true), &mut layer);
            rune_kit::root(settings, layer)
        }
        let (commands, _) = crate::model::commands::channel();
        let mut state = UiState::new(commands);
        state.settings_tab = super::super::SettingsTab::General;
        let mut rt = rune::Runtime::new(state, view, (1080.0, 800.0), rune::Env::new(true, 1.0));
        for (step, key) in ["settings-nav-Computers", "computers-pair"]
            .into_iter()
            .enumerate()
        {
            for frame in 0..20 {
                rt.frame((step * 20 + frame) as f64 / 60.0);
            }
            let (x, y) = rt.find(key).expect("visible pairing entry");
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
        }
        rt.frame(0.7);
        assert_eq!(rt.state.pairing_target.as_deref(), Some(""));
        assert!(rt.state.pairing_url_input.focused);
    }
}
