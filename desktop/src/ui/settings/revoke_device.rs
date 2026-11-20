//! Confirmation stays on the selected inbound device's row.
use crate::model::commands::Command;
use crate::platform::paired_devices::List;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;
use rune::El;
use rune_kit::{ButtonVariant, Kit};

#[derive(Default)]
pub enum State {
    #[default]
    Idle,
    Confirm(String),
    Pending(String),
}

impl State {
    pub fn pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }

    pub fn cancel_confirmation(&mut self) {
        if matches!(self, Self::Confirm(_)) {
            *self = Self::Idle;
        }
    }
}

impl UiState {
    fn press_local_revoke(&mut self, id: &str, confirm: bool) {
        if self.local_revoke.pending()
            || !matches!(&self.local_pairings, List::Ready(rows) if rows.iter().any(|r| r.id == id))
        {
            return;
        }
        if confirm {
            if !matches!(&self.local_revoke, State::Confirm(target) if target == id) {
                return;
            }
            self.local_revoke = State::Pending(id.to_owned());
            self.send(Command::RevokeLocalPairing(id.to_owned()));
        } else {
            self.local_revoke = State::Confirm(id.to_owned());
        }
    }

    pub fn finish_local_revoke(&mut self, id: String, result: Result<(), String>) {
        if !matches!(&self.local_revoke, State::Pending(pending) if pending == &id) {
            return;
        }
        self.local_revoke = State::Idle;
        match result {
            Ok(()) => {
                if let List::Ready(rows) = &mut self.local_pairings {
                    rows.retain(|r| r.id != id);
                }
                self.refresh_local_pairings();
            }
            Err(err) => {
                self.toast_error(
                    tr_ref(self.cfg.general.language, "Could not revoke device access"),
                    Some(err),
                );
            }
        }
    }
}

pub(super) fn control(s: &UiState, k: &Kit, id: &str) -> El {
    let lang = s.cfg.general.language;
    let confirm = matches!(&s.local_revoke, State::Confirm(target) if target == id);
    let pending = matches!(&s.local_revoke, State::Pending(target) if target == id);
    let id = id.to_owned();
    let mut button = super::button(tr_ref(
        lang,
        if pending {
            "Revoking…"
        } else if confirm {
            "Confirm revoke"
        } else {
            "Revoke access"
        },
    ));
    if confirm {
        button = button.variant(ButtonVariant::Danger);
    }
    let primary = button
        .disabled(s.local_revoke.pending())
        .on_press(move |s: &mut UiState| s.press_local_revoke(&id, confirm))
        .build(k);
    if confirm {
        rune::row().center_y().gap(6.0).child(primary).child(
            super::button(tr_ref(lang, "Cancel"))
                .on_press(|s: &mut UiState| s.local_revoke.cancel_confirmation())
                .build(k),
        )
    } else {
        primary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands;
    use crate::platform::paired_devices::Client;

    #[test]
    fn only_confirming_the_selected_device_revokes_it_once_and_failures_keep_it() {
        let (tx, mut rx) = commands::channel();
        let mut s = UiState::new(tx);
        let clients = ["phone", "mini"]
            .map(|id| Client {
                id: id.into(),
                name: id.into(),
                kind: None,
                last_seen_unix_ms: None,
            })
            .to_vec();
        let mut revocations = || {
            std::iter::from_fn(|| rx.try_recv().ok())
                .filter_map(|cmd| match cmd {
                    Command::RevokeLocalPairing(id) => Some(id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        s.local_pairings = List::Ready(clients.clone());
        s.press_local_revoke("phone", false);
        s.press_local_revoke("phone", false); // A repeated first click is still not confirmation.
        s.press_local_revoke("mini", true);
        s.local_revoke.cancel_confirmation();
        s.press_local_revoke("phone", true);
        assert!(revocations().is_empty());
        s.press_local_revoke("mini", false);
        s.press_local_revoke("mini", true);
        s.press_local_revoke("mini", true);
        assert_eq!(revocations(), ["mini"]);
        s.finish_local_revoke("phone".into(), Ok(()));
        assert!(s.local_revoke.pending());
        s.finish_local_revoke("mini".into(), Err("refused".into()));
        assert!(!s.local_revoke.pending());
        assert_eq!(s.local_pairings, List::Ready(clients));
        assert!(revocations().is_empty());
    }
}
