//! The clients signed in to this computer, including phones and other Gateways.
use super::{button, group_action, row};
use crate::model::commands::Command;
use crate::platform::paired_devices::List;
use crate::platform::pairing_invite::Invite;
use crate::ui::copy::tr_ref;
use crate::ui::{ago, now_ms, UiState};
use rune::{col, el, text, El, Tx};
use rune_kit::Kit;
use std::time::{Duration, Instant};

pub(crate) struct InvitePanel {
    id: u64,
    loading: bool,
    next_read: Instant,
    snapshot: Option<Invite>,
}

impl UiState {
    pub fn refresh_local_pairings(&mut self) {
        if self.local_pairings != List::Loading && !self.local_revoke.pending() {
            self.local_pairings = List::Loading;
            self.send(Command::RefreshLocalPairings);
        }
    }

    fn toggle_local_invite(&mut self) {
        if self.local_invite.take().is_some() {
            self.refresh_local_pairings();
            return;
        }
        let id = self.next_token;
        self.next_token += 1;
        self.local_invite = Some(InvitePanel {
            id,
            loading: true,
            next_read: Instant::now(),
            snapshot: None,
        });
        self.send(Command::ReadLocalInvite(id));
    }

    pub(crate) fn receive_local_invite(&mut self, id: u64, result: Option<Invite>) {
        let Some(panel) = self.local_invite.as_mut().filter(|p| p.id == id) else {
            return;
        };
        let finished = panel.snapshot.as_ref().is_some_and(|v| v.pending.is_some())
            && result.as_ref().is_some_and(|v| v.pending.is_none());
        panel.loading = false;
        panel.next_read = Instant::now() + Duration::from_secs(2);
        panel.snapshot = result;
        if finished {
            self.refresh_local_pairings();
        }
    }

    pub(crate) fn poll_local_invite(&mut self, tx: &mut Tx) -> bool {
        let Some(panel) = self.local_invite.as_mut() else {
            return false;
        };
        let settings_visible = match self.active_plugin_page.as_deref() {
            Some(plugin) => plugin == "settings",
            None if self.mode == crate::ui::Mode::Session => self.tiles.iter().any(|tile| {
                matches!(&tile.content, crate::ui::TileContent::Custom { kind, .. } if kind == "settings")
            }),
            None => self.mode == crate::ui::Mode::Settings,
        };
        if !settings_visible || self.settings_tab != crate::ui::SettingsTab::Computers {
            self.local_invite = None;
            return true;
        }
        if panel.loading {
            return false;
        }
        let wait = panel.next_read.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            panel.loading = true;
            let id = panel.id;
            self.send(Command::ReadLocalInvite(id));
            return true;
        }
        tx.wake_after(wait);
        false
    }
}

pub(super) fn group(s: &UiState, k: &Kit) -> El {
    let lang = s.cfg.general.language;
    let tr = |key| tr_ref(lang, key);
    let actions = rune::row()
        .gap(8.0)
        .flex_wrap()
        .child(
            button(tr("Refresh"))
                .disabled(s.local_pairings == List::Loading || s.local_revoke.pending())
                .on_press(UiState::refresh_local_pairings)
                .build(k),
        )
        .child(
            button(tr(if s.local_invite.is_some() {
                "Close"
            } else {
                "Pair a device"
            }))
            .on_press(UiState::toggle_local_invite)
            .build(k)
            .key("local-invite-toggle"),
        );
    let status = match &s.local_pairings {
        List::Unavailable => "Local Gateway unavailable",
        List::Failed => "Could not load paired devices",
        List::Idle | List::Loading => "Loading…",
        List::Ready(devices) if devices.is_empty() => "No devices paired to this computer",
        List::Ready(_) => "",
    };
    let rows = if let List::Ready(devices) = &s.local_pairings {
        devices
            .iter()
            .map(|d| {
                let kind = tr(if d.kind.as_deref() == Some("gateway") {
                    "Gateway"
                } else {
                    "App"
                });
                let seen = d
                    .last_seen_unix_ms
                    .filter(|t| *t > 0)
                    .map(|t| format!("{} · {}", tr("Last seen"), ago(t, now_ms(), lang)))
                    .unwrap_or_default();
                row(
                    k,
                    &format!("local-client-{}", d.id),
                    &d.name,
                    &if seen.is_empty() {
                        kind.to_owned()
                    } else {
                        format!("{kind} · {seen}")
                    },
                    super::revoke_device::control(s, k, &d.id),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut rows = if status.is_empty() {
        rows
    } else {
        vec![el().child(text(tr(status)).font(k.ty.ui).color(k.c.text_muted))]
    };
    if let Some(panel) = &s.local_invite {
        let body = match &panel.snapshot {
            Some(invite) => {
                let mut content = col().gap(8.0).w_full();
                if let Some(code) = invite.pending.as_ref().filter(|c| c.expires_ms > now_ms()) {
                    content = content
                        .child(text(code.name.clone()).font(k.ty.ui_medium))
                        .child(text(tr("Enter this code on the requesting device.")).font(k.ty.ui))
                        .child(
                            text(code.code.clone())
                                .font(rune::TextStyle::new(26.0, 650.0).lh(34.0)),
                        );
                } else {
                    content = content
                        .child(
                            text(tr(
                                "Scan with Muqun, or enter this address on the other computer.",
                            ))
                            .font(k.ty.ui)
                            .wrap(),
                        )
                        .child(invite.qr_view())
                        .child(text(invite.address.clone()).font(k.ty.ui).wrap());
                }
                content
            }
            None => text(tr(if panel.loading {
                "Loading…"
            } else {
                "Could not load pairing information"
            }))
            .font(k.ty.ui),
        };
        rows.insert(0, body);
    }
    group_action(
        k,
        "local-paired-devices",
        tr("Devices paired to this computer"),
        actions,
        rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_pairing_stays_in_settings_and_an_old_response_cannot_reopen_it() {
        fn view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let mut layer = Vec::new();
            let page = super::super::view(s, &Kit::neutral(true), &mut layer);
            rune_kit::root(page, layer)
        }
        let (send, mut recv) = crate::model::commands::channel();
        let mut s = UiState::new(send);
        s.mode = crate::ui::Mode::Settings;
        s.settings_tab = crate::ui::SettingsTab::Computers;
        let mut rt = rune::Runtime::new(s, view, (1080.0, 800.0), rune::Env::new(true, 1.0));
        for step in 0..2 {
            for frame in 0..15 {
                rt.frame((step * 15 + frame) as f64 / 60.0);
            }
            let (x, y) = rt.find("local-invite-toggle").unwrap();
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
        }
        rt.frame(0.6);
        let id = std::iter::from_fn(|| recv.try_recv().ok())
            .find_map(|cmd| match cmd {
                Command::ReadLocalInvite(id) => Some(id),
                _ => None,
            })
            .expect("the settings button queried this Gateway");
        assert!(rt.state.local_invite.is_none());
        rt.state.receive_local_invite(id, None);
        assert!(rt.state.local_invite.is_none());
        assert_eq!(rt.state.mode, crate::ui::Mode::Settings);
        assert!(rt.state.term.tabs.is_empty());
    }
}
