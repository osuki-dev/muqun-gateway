//! The native first-run Gateway installer. Download totals come from the installer.
use crate::platform::gateway_bootstrap::{self, Status};
use rune::El;
use rune_kit::{Button, ButtonVariant, Kit};

use super::{
    copy::tr_ref,
    download_panel::{DownloadPanel, DownloadProgress},
    UiState,
};

pub(super) fn required(status: Status) -> Option<Status> {
    match status {
        Status::Unavailable | Status::Existing | Status::Ready => None,
        other => Some(other),
    }
}

impl UiState {
    pub(super) fn show_gateway_setup(&mut self, status: Status) {
        self.gateway_setup = Some(status);
    }
    pub(super) fn receive_gateway_setup(&mut self, status: Status) {
        self.gateway_setup = if status == Status::Ready && self.gateway_setup.is_some() {
            Some(Status::Ready)
        } else {
            required(status)
        };
    }
    fn start_gateway_setup(&mut self) {
        if matches!(
            self.gateway_setup,
            Some(Status::NeedsConsent | Status::Failed | Status::Interrupted)
        ) {
            self.gateway_setup = Some(Status::Installing);
            gateway_bootstrap::retry();
        }
    }
    fn finish_gateway_setup(&mut self) {
        if self.gateway_setup == Some(Status::Ready) {
            self.gateway_setup = None;
        }
    }
}

pub(super) fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    let Some(status) = s.gateway_setup.as_ref() else {
        return;
    };
    let lang = s.cfg.general.language;
    let tr = |key| tr_ref(lang, key);
    let title = tr(match status {
        Status::NeedsConsent | Status::Checking => "Set up Muqun Gateway",
        Status::Ready => "Gateway is ready",
        Status::Failed | Status::Interrupted => "Gateway setup needs attention",
        _ => "Installing Muqun Gateway",
    });
    let body = tr(match status {
        Status::NeedsConsent => "Desktop needs Gateway on this computer to open your workspace.",
        Status::Ready => "This computer is ready to pair.",
        _ => status.message(),
    });
    let secondary = (!matches!(status, Status::Ready)).then(|| {
        Button::new(tr("Quit Muqun"))
            .variant(ButtonVariant::Ghost)
            .on_press_tx(|_: &mut UiState, tx| tx.quit())
            .build(k)
            .key("gateway-quit")
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
                .on_press(UiState::finish_gateway_setup)
                .build(k)
                .key("gateway-done"),
        ),
        _ => None,
    };
    let progress = match *status {
        Status::Downloading { received, total } => {
            Some(DownloadProgress::Bytes { received, total })
        }
        Status::Checking | Status::Installing | Status::Activating | Status::Starting => {
            Some(DownloadProgress::Busy)
        }
        Status::Verifying => Some(DownloadProgress::Verifying),
        Status::Ready => Some(DownloadProgress::Complete),
        _ => None,
    };
    let panel = DownloadPanel {
        title,
        message: body,
        progress,
        secondary,
        primary,
    };
    if *status == Status::Ready {
        panel.mount(s, k, layer, UiState::finish_gateway_setup);
    } else {
        panel.mount_required(s, k, layer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_keeps_the_gate_through_failure_and_retry_until_ready() {
        fn view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let mut layer = Vec::new();
            mount(s, &Kit::neutral(true), &mut layer);
            rune_kit::root(rune::el(), layer)
        }
        let (commands, _) = crate::model::commands::channel();
        let mut state = UiState::new(commands);
        state.receive_gateway_setup(Status::Checking);
        state.finish_gateway_setup();
        assert_eq!(state.gateway_setup, Some(Status::Checking));
        state.receive_gateway_setup(Status::NeedsConsent);
        let mut rt = rune::Runtime::new(state, view, (900.0, 700.0), rune::Env::new(true, 1.0));
        let mut time = 0.0;
        let mut click = |rt: &mut rune::Runtime<UiState>, key: &str| {
            rt.invalidate();
            for _ in 0..25 {
                time += 1.0 / 60.0;
                rt.frame(time);
            }
            let (x, y) = rt
                .find(key)
                .unwrap_or_else(|| panic!("missing {key} in {:?}", rt.state.gateway_setup));
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            time += 1.0 / 60.0;
            rt.frame(time);
        };
        click(&mut rt, "gateway-install");
        assert_eq!(rt.state.gateway_setup, Some(Status::Installing));
        rt.state.receive_gateway_setup(Status::Failed);
        rt.state.finish_gateway_setup();
        assert_eq!(rt.state.gateway_setup, Some(Status::Failed));
        click(&mut rt, "gateway-install");
        rt.state.receive_gateway_setup(Status::Ready);
        click(&mut rt, "gateway-done");
        assert_eq!(rt.state.gateway_setup, None);
    }
}
