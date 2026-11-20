//! An installed Desktop can update in the background and restart on request.
use super::{
    copy::tr_ref,
    download_panel::{DownloadPanel, DownloadProgress},
    Slot, ToastLevel, UiState,
};
use crate::{
    model::commands::Command,
    platform::{
        installer::Phase,
        update::{self, Status},
    },
};
use rune::El;
use rune_kit::{Button, ButtonVariant, Kit};

impl UiState {
    pub(super) fn receive_desktop_update(&mut self, status: Status) {
        if !self.desktop_update_panel {
            let notice = match &status {
                Status::Ready { .. } => {
                    Some((ToastLevel::Info, "Restart to use the installed update."))
                }
                Status::InstallFailed(_) => Some((ToastLevel::Error, "Desktop update failed")),
                _ => None,
            };
            if let Some((level, message)) = notice {
                self.notify(
                    level,
                    "Muqun Desktop",
                    Some(tr_ref(self.cfg.general.language, message).to_owned()),
                );
            }
        }
        self.desktop_update = status;
    }

    pub(super) fn start_desktop_update(&mut self) {
        let (Status::Available(version) | Status::InstallFailed(version)) = &self.desktop_update
        else {
            return;
        };
        let version = version.clone();
        self.desktop_update_panel = true;
        self.desktop_update = Status::Installing {
            version: version.clone(),
            phase: None,
        };
        self.send(Command::InstallDesktopUpdate(version));
    }

    pub(super) fn show_desktop_update(&mut self) {
        self.desktop_update_panel = true;
    }
    fn hide_desktop_update(&mut self) {
        self.desktop_update_panel = false;
    }

    fn update_restart_safe(&self) -> bool {
        let has_local_edits = !self.staged.is_empty()
            || self
                .deferred_attachments
                .values()
                .any(|files| !files.is_empty())
            || self.tiles.iter().any(|tile| !tile.slot.staged.is_empty())
            || self.computer_data.values().any(|computer| {
                !computer.staged.is_empty()
                    || computer.pending_submit.is_some()
                    || computer.pending_start.is_some()
                    || computer
                        .focused_slot
                        .as_ref()
                        .is_some_and(|slot| !slot.staged.is_empty())
                    || computer
                        .tiles
                        .iter()
                        .any(|tile| !tile.slot.staged.is_empty())
            });
        !has_local_edits
            && self.pending_submit.is_none()
            && self.pending_start.is_none()
            && self.pending_file_targets.is_empty()
            && self.pending_image_uploads.is_empty()
            && self.voice.is_none()
            && (self.hub_label != "standalone" || self.busy_since.is_empty())
    }

    fn restart_updated_desktop(&mut self) {
        if !matches!(self.desktop_update, Status::Ready { .. })
            || self.restart_checkpoint_pending
            || !self.update_restart_safe()
        {
            return;
        }
        self.persist_draft(Slot::Start);
        self.persist_draft(Slot::Session);
        self.restart_checkpoint_pending = true;
        self.send(Command::CheckpointForUpdateRestart);
    }

    pub(super) fn restart_checkpoint_completed(&mut self, saved: bool) {
        self.restart_checkpoint_pending = false;
        if !self.update_restart_safe() {
            return;
        }
        let Status::Ready {
            executable,
            version,
        } = &self.desktop_update
        else {
            return;
        };
        if saved && update::request_restart(executable.clone(), version).is_ok() {
            self.restart_on_frame = true;
        } else {
            self.notify(
                ToastLevel::Error,
                tr_ref(self.cfg.general.language, "Could not restart Desktop"),
                None,
            );
        }
    }
}

pub(super) fn phase_label(phase: Option<Phase>) -> &'static str {
    match phase {
        None => "Preparing package…",
        Some(Phase::Downloading { .. }) => "Downloading…",
        Some(Phase::Verifying) => "Verifying…",
        Some(Phase::Installing | Phase::Starting) => "Installing…",
    }
}

pub(super) fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    if !s.desktop_update_panel {
        return;
    }
    let tr = |key| tr_ref(s.cfg.general.language, key);
    let (version, body, progress, secondary, primary) = match &s.desktop_update {
        Status::Installing { version, phase } => {
            let progress = match phase {
                Some(Phase::Downloading { received, total }) => DownloadProgress::Bytes {
                    received: *received,
                    total: *total,
                },
                Some(Phase::Verifying) => DownloadProgress::Verifying,
                _ => DownloadProgress::Busy,
            };
            (
                version,
                phase_label(*phase),
                Some(progress),
                "Run in background",
                None,
            )
        }
        Status::InstallFailed(version) => (
            version,
            "Desktop update failed",
            None,
            "Close",
            Some(Button::new(tr("Retry")).on_press(UiState::start_desktop_update)),
        ),
        Status::Ready { version, .. } => {
            let safe = s.update_restart_safe();
            let message = if safe {
                "Restart to use the installed update."
            } else {
                "Finish active work and attachments before restarting."
            };
            (
                version,
                message,
                Some(DownloadProgress::Complete),
                "Later",
                Some(
                    Button::new(tr("Restart now"))
                        .disabled(!safe || s.restart_checkpoint_pending)
                        .on_press(UiState::restart_updated_desktop),
                ),
            )
        }
        _ => return,
    };
    let title = format!("Muqun Desktop {version}");
    DownloadPanel {
        title: &title,
        message: tr(body),
        progress,
        secondary: Some(
            Button::new(tr(secondary))
                .variant(ButtonVariant::Ghost)
                .on_press(UiState::hide_desktop_update)
                .build(k)
                .key("desktop-update-hide"),
        ),
        primary: primary.map(|b| {
            b.variant(ButtonVariant::Submit)
                .build(k)
                .key("desktop-update-primary")
        }),
    }
    .mount(s, k, layer, UiState::hide_desktop_update);
}
