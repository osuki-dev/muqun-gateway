//! The native first-run Gateway installer. Download totals come from the installer.
use crate::platform::gateway_bootstrap::{self, Status};
use rune::El;
use rune_kit::{Button, ButtonVariant, Kit};

use super::{
    copy::tr_ref,
    download_panel::{DownloadPanel, DownloadProgress},
    UiState,
};

pub(super) fn running(status: &Status) -> bool {
    matches!(
        status,
        Status::Installing
            | Status::Downloading { .. }
            | Status::Verifying
            | Status::Activating
            | Status::Starting
    )
}

impl UiState {
    pub(super) fn show_gateway_setup(&mut self, status: Status) {
        self.gateway_setup = Some(if matches!(status, Status::Deferred) {
            Status::NeedsConsent
        } else {
            status
        });
    }

    pub(super) fn receive_gateway_setup(&mut self, status: Status) {
        if status == Status::NeedsConsent || self.gateway_setup.is_some() {
            self.gateway_setup = match status {
                Status::Deferred
                | Status::Disabled
                | Status::Remote
                | Status::Existing
                | Status::Unavailable => None,
                other => Some(other),
            };
        } else if matches!(status, Status::Ready | Status::Failed | Status::Interrupted) {
            let lang = self.cfg.general.language;
            self.notify(
                if status == Status::Ready {
                    super::ToastLevel::Info
                } else {
                    super::ToastLevel::Error
                },
                tr_ref(lang, "Local Gateway setup"),
                Some(tr_ref(lang, status.message()).into()),
            );
        }
    }

    fn start_gateway_setup(&mut self) {
        if self.gateway_setup.as_ref().is_some_and(|s| {
            matches!(
                s,
                Status::NeedsConsent | Status::Failed | Status::Interrupted
            )
        }) {
            self.gateway_setup = Some(Status::Installing);
            gateway_bootstrap::retry(&self.cfg);
        }
    }

    fn close_gateway_setup(&mut self) {
        if self.gateway_setup.take() == Some(Status::NeedsConsent) {
            gateway_bootstrap::defer();
        }
    }
}

pub(super) fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    let Some(status) = s.gateway_setup.as_ref() else {
        return;
    };
    if s.splash.is_some() {
        return;
    }
    let lang = s.cfg.general.language;
    let tr = |key| tr_ref(lang, key);
    let active = running(status);
    let title = tr(match status {
        Status::NeedsConsent => "Install Muqun Gateway?",
        Status::Ready => "Gateway is ready",
        Status::Failed | Status::Interrupted => "Gateway setup needs attention",
        _ => "Installing Muqun Gateway",
    });
    let body = tr(match status {
        Status::NeedsConsent => "Connect your phone and other computers to this Desktop. You can also install Gateway later in Settings → Diagnostics.",
        Status::Ready => "This computer is ready to pair.",
        _ => status.message(),
    });
    let secondary = match status {
        Status::NeedsConsent => Some("Later"),
        Status::Failed | Status::Interrupted => Some("Close"),
        _ if active => Some("Run in background"),
        _ => None,
    };
    let secondary = secondary.map(|key| {
        Button::new(tr(key))
            .variant(ButtonVariant::Ghost)
            .on_press(UiState::close_gateway_setup)
            .build(k)
            .key("gateway-later")
    });
    let primary = match status {
        Status::NeedsConsent | Status::Failed | Status::Interrupted => Some(
            Button::new(tr(if *status == Status::NeedsConsent {
                "Download & install"
            } else {
                "Retry"
            }))
            .variant(ButtonVariant::Submit)
            .on_press(UiState::start_gateway_setup)
            .build(k)
            .key("gateway-install"),
        ),
        Status::Ready => Some(
            Button::new(tr("Done"))
                .variant(ButtonVariant::Submit)
                .on_press(UiState::close_gateway_setup)
                .build(k)
                .key("gateway-done"),
        ),
        _ => None,
    };
    let progress = match *status {
        Status::Downloading { received, total } => {
            Some(DownloadProgress::Bytes { received, total })
        }
        _ if active => Some(DownloadProgress::Busy),
        _ => None,
    };
    DownloadPanel {
        title,
        message: body,
        progress,
        secondary,
        primary,
    }
    .mount(s, k, layer, UiState::close_gateway_setup);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_launch_can_defer_reopen_and_background_a_download() {
        fn view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let mut layer = Vec::new();
            mount(s, &Kit::neutral(true), &mut layer);
            rune_kit::root(rune::el(), layer)
        }
        let (commands, _) = crate::model::commands::channel();
        let mut state = UiState::new(commands);
        state.splash = None;
        state.receive_gateway_setup(Status::NeedsConsent);
        let mut rt = rune::Runtime::new(state, view, (900.0, 700.0), rune::Env::new(true, 1.0));
        let mut clock = 0.0;
        let mut click = |rt: &mut rune::Runtime<UiState>, key: &str| {
            for _ in 0..25 {
                clock += 1.0 / 60.0;
                rt.frame(clock);
            }
            let (x, y) = rt.find(key).expect("installer action is accessible");
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
        };
        click(&mut rt, "gateway-later");
        assert_eq!(rt.state.gateway_setup, None);
        rt.state.show_gateway_setup(Status::Deferred);
        click(&mut rt, "gateway-install");
        assert_eq!(rt.state.gateway_setup, Some(Status::Installing));
        let progress = Status::Downloading {
            received: 100,
            total: 300,
        };
        rt.state.receive_gateway_setup(progress.clone());
        click(&mut rt, "gateway-later");
        rt.state.receive_gateway_setup(Status::Verifying);
        assert_eq!(rt.state.gateway_setup, None);
        rt.state.show_gateway_setup(progress);
        rt.state.receive_gateway_setup(Status::Failed);
        click(&mut rt, "gateway-install");
        rt.state.receive_gateway_setup(Status::Ready);
        click(&mut rt, "gateway-done");
        assert_eq!(rt.state.gateway_setup, None);
    }
}
