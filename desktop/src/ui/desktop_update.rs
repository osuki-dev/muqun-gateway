//! Compact bottom-right update toast/card: an installed Desktop checks on
//! launch (and periodically), pops up cleanly in the bottom-right corner when
//! a new release is ready, downloads in place, and restarts only on request.
use std::sync::Arc;

use rune::{col, el, icon, row, text, Color, El, LoopSpec, Spring, SurfaceFit, Transition};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use super::{copy::tr_ref, icons, parts::glyphs, Mode, SettingsTab, Slot, ToastLevel, UiState};
use crate::{
    branding::APP_MARK_PNG,
    model::{commands::Command, viewer::human_size},
    platform::{
        installer::Phase,
        update::{self, Status},
    },
};

const CARD_W: f32 = 332.0;

impl UiState {
    pub(super) fn receive_desktop_update(&mut self, status: Status) {
        match &status {
            Status::Available(version) => {
                if self.cfg.general.announced_desktop_update.as_ref() != Some(version) {
                    self.edit(crate::config::ConfigEdit::AnnouncedDesktopUpdate(
                        version.clone(),
                    ));
                    self.desktop_update_panel = true;
                }
            }
            Status::Ready { .. } | Status::InstallFailed(_) => {
                self.desktop_update_panel = true;
            }
            _ => {}
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

fn slim_meter(
    k: &Kit,
    accent: Color,
    accent_end: Color,
    ratio: Option<f32>,
    motion: bool,
    label: &str,
) -> El {
    let tip = accent_end.mix(k.c.white, 0.45);
    let mut track = row()
        .key("desktop-update-track")
        .role("progressbar")
        .label(label)
        .w_full()
        .h(6.0)
        .radius(3.0)
        .fill(k.c.layer_02)
        .clip_soft(0.0);
    let mut beam = el()
        .key("desktop-update-beam")
        .h_full()
        .radius(3.0)
        .linear_gradient(
            std::f32::consts::FRAC_PI_2,
            [(0.0, accent), (0.72, accent_end), (1.0, tip)],
        );
    if let Some(ratio) = ratio {
        let v = ratio.clamp(0.0, 1.0);
        track = track
            .range_value(f64::from(v), 0.0, 1.0)
            .a11y_value(format!("{:.0}%", (v * 100.0).floor()));
        if v <= 0.0 {
            return track;
        }
        if motion && v < 1.0 {
            let sweep = el()
                .w_frac(0.36)
                .h_full()
                .linear_gradient(
                    std::f32::consts::FRAC_PI_2,
                    [
                        (0.0, tip.alpha(0.0)),
                        (0.65, tip.alpha(0.75)),
                        (0.88, k.c.white.alpha(0.92)),
                        (1.0, tip.alpha(0.0)),
                    ],
                )
                .looping(LoopSpec::slide(1.45, [-1.0, 0.0], [3.1, 0.0]));
            beam = beam.child(sweep).token_motion(k.motion.disclosure);
        }
        return track.child(beam.w_frac(v).clip_soft(0.0));
    }
    beam = beam.w_frac(0.30);
    if motion {
        beam = beam.looping(LoopSpec::slide(1.55, [-1.0, 0.0], [3.6, 0.0]));
    }
    track.child(beam)
}

fn mark_badge(s: &UiState, k: &Kit, tone: Color, ready: bool, active: bool) -> El {
    let look = s.look();
    let inner = if ready {
        el().size(32.0, 32.0)
            .center()
            .radius(k.radius.md)
            .fill(tone.alpha(0.16))
            .border(1.0, tone.alpha(0.35))
            .child(icon(&glyphs::DONE, 16.0).color(tone))
    } else if let Some(images) = s.images.as_ref() {
        super::art::picture(images, Arc::<[u8]>::from(APP_MARK_PNG), SurfaceFit::Fill)
            .size(32.0, 32.0)
            .radius(k.radius.md)
            .clip_rounded(k.radius.md)
            .border(1.0, tone.alpha(0.32))
    } else {
        el().size(32.0, 32.0)
            .center()
            .radius(k.radius.md)
            .fill(tone.alpha(0.16))
            .border(1.0, tone.alpha(0.32))
            .child(icon(&glyphs::COMPUTER, 16.0).color(tone))
    };
    if active {
        look.pulse(inner)
    } else {
        inner
    }
}

pub(super) fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    if !s.desktop_update_panel {
        return;
    }
    let tr = |key| tr_ref(s.cfg.general.language, key);
    let look = s.look();
    let motion = !(s.cfg.appearance.reduced_motion || s.system_reduced_motion.get());
    let [c0, c1, _] = look.highlight_colors(k);

    let (
        version,
        subtitle,
        tone,
        tone_end,
        ready,
        active_anim,
        progress_row,
        show_details,
        secondary_label,
        primary,
    ) = match &s.desktop_update {
        Status::Available(version) => (
            version.as_str(),
            tr("New version available:").trim_end_matches([':', '：']),
            c0,
            c1,
            false,
            true,
            None,
            true,
            tr("Later"),
            Some(
                Button::new(tr("Update"))
                    .variant(ButtonVariant::Submit)
                    .control_size(ControlSize::Small)
                    .on_press(UiState::start_desktop_update)
                    .build(k)
                    .key("desktop-update-primary"),
            ),
        ),
        Status::Installing { version, phase } => {
            let label = tr(phase_label(*phase));
            let (ratio, pct, size_text) = match phase {
                Some(Phase::Downloading { received, total }) if *total > 0 => {
                    let done = (*received).min(*total);
                    let r = (done as f64 / *total as f64) as f32;
                    let p = u128::from(done) * 100 / u128::from(*total);
                    (
                        Some(r),
                        Some(format!("{p}%")),
                        Some(format!(
                            "{} / {}",
                            human_size(*received),
                            human_size(*total)
                        )),
                    )
                }
                Some(Phase::Downloading { received, .. }) => {
                    (None, None, Some(human_size(*received)))
                }
                _ => (None, None, None),
            };
            let mut progress_block = col()
                .w_full()
                .gap(5.0)
                .child(slim_meter(k, c0, c1, ratio, motion, label));
            if pct.is_some() || size_text.is_some() {
                let mut meta = row().w_full().center_y().gap(6.0);
                if let Some(p) = pct {
                    meta = meta.child(text(p).font(k.ty.mono).color(k.c.text_base));
                }
                meta = meta.child(el().grow(1.0));
                if let Some(sz) = size_text {
                    meta = meta.child(text(sz).font(k.ty.mono).color(k.c.text_faint));
                }
                progress_block = progress_block.child(meta);
            }
            (
                version.as_str(),
                label,
                c0,
                c1,
                false,
                true,
                Some(progress_block),
                false,
                tr("Run in background"),
                None,
            )
        }
        Status::Ready { version, .. } => {
            let safe = s.update_restart_safe();
            let msg = if safe {
                tr("Restart to use the installed update.")
            } else {
                tr("Finish active work and attachments before restarting.")
            };
            let done_bar = col().w_full().child(slim_meter(
                k,
                k.c.success.fg,
                k.c.success.fg.mix(k.c.white, 0.25),
                Some(1.0),
                false,
                msg,
            ));
            (
                version.as_str(),
                msg,
                k.c.success.fg,
                k.c.success.fg.mix(c0, 0.35),
                true,
                false,
                Some(done_bar),
                false,
                tr("Later"),
                Some(
                    Button::new(tr("Restart now"))
                        .variant(ButtonVariant::Submit)
                        .control_size(ControlSize::Small)
                        .disabled(!safe || s.restart_checkpoint_pending)
                        .on_press(UiState::restart_updated_desktop)
                        .build(k)
                        .key("desktop-update-primary"),
                ),
            )
        }
        Status::InstallFailed(version) => (
            version.as_str(),
            tr("Desktop update failed"),
            k.c.danger.fg,
            k.c.danger.fg,
            false,
            false,
            None,
            true,
            tr("Close"),
            Some(
                Button::new(tr("Retry"))
                    .variant(ButtonVariant::Submit)
                    .control_size(ControlSize::Small)
                    .on_press(UiState::start_desktop_update)
                    .build(k)
                    .key("desktop-update-primary"),
            ),
        ),
        _ => return,
    };

    let version_pill = row()
        .px(6.0)
        .py(1.0)
        .radius(k.radius.sm)
        .fill(tone.alpha(0.15))
        .border(1.0, tone.alpha(0.28))
        .child(text(format!("v{version}")).font(k.ty.caption()).color(tone));

    let header = row()
        .w_full()
        .items_start()
        .gap(10.0)
        .child(mark_badge(s, k, tone, ready, active_anim))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(3.0)
                .child(
                    row()
                        .w_full()
                        .center_y()
                        .gap(6.0)
                        .flex_wrap()
                        .child(
                            text("Muqun Desktop")
                                .font(k.ty.ui_medium)
                                .color(k.c.text_base),
                        )
                        .child(version_pill),
                )
                .child(
                    text(subtitle)
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .w_full()
                        .wrap(),
                ),
        )
        .child(
            icons::header_button(k.glyph.close, tr("Close"))
                .on_press(UiState::hide_desktop_update)
                .build(k)
                .key("desktop-update-close"),
        );

    let mut actions = row().w_full().center_y().gap(6.0);
    if show_details {
        actions = actions.child(
            Button::new(tr("Details"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| {
                    s.go(Mode::Settings);
                    super::settings::open(s, SettingsTab::About);
                })
                .build(k)
                .key("desktop-update-details"),
        );
    }
    actions = actions.child(el().grow(1.0)).child(
        Button::new(secondary_label)
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(UiState::hide_desktop_update)
            .build(k)
            .key("desktop-update-hide"),
    );
    if let Some(primary) = primary {
        actions = actions.child(primary);
    }

    let mut body = col()
        .w_full()
        .px(14.0)
        .pt(12.0)
        .pb(12.0)
        .gap(10.0)
        .child(header);
    if let Some(progress) = progress_row {
        body = body.child(progress);
    }
    body = body.child(actions);

    let accent_strip = el()
        .w_full()
        .h(2.0)
        .linear_gradient(
            std::f32::consts::FRAC_PI_2,
            [
                (0.0, tone.alpha(0.15)),
                (0.35, tone),
                (0.75, tone_end),
                (1.0, tone_end.alpha(0.12)),
            ],
        )
        .a11y_hidden();

    let inner = col()
        .key("desktop-update-card")
        .role("status")
        .label(format!("Muqun Desktop {version}"))
        .w(CARD_W)
        .max_w(CARD_W)
        .clip_rounded(k.radius.xl)
        .child(accent_strip)
        .child(body);

    let card = look
        .floating_surface(k, inner, k.radius.xl)
        .pin(None, None, Some(16.0), Some(16.0))
        .transition(
            Transition::opacity()
                .combined(Transition::move_edge(rune::Edge::Bottom))
                .combined(Transition::scale(0.95)),
        )
        .motion(if motion {
            Spring::new(0.28, 0.04)
        } else {
            Spring::NONE
        });

    layer.push(card);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{commands, inbox::UiAction};

    #[test]
    fn scheduled_checks_coalesce_and_announce_each_release_once() {
        let (sender, mut commands) = commands::channel();
        let mut ui = UiState::new(sender);
        while commands.try_recv().is_ok() {}
        ui.on_action(UiAction::CheckDesktopUpdateIfIdle);
        ui.on_action(UiAction::CheckDesktopUpdateIfIdle);
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::CheckDesktopUpdate)
        ));
        assert!(commands.try_recv().is_err());

        // First announcement pops up the compact bottom-right card automatically.
        ui.receive_desktop_update(Status::Available("1.1.0".into()));
        assert_eq!(ui.mode, Mode::Home);
        assert!(ui.desktop_update_panel);

        // Once dismissed, repeated periodic checks for the same version stay quiet.
        ui.hide_desktop_update();
        assert!(!ui.desktop_update_panel);
        ui.receive_desktop_update(Status::Available("1.1.0".into()));
        assert!(!ui.desktop_update_panel);

        // Legacy notification deep-link still routes cleanly to Settings -> About.
        ui.open_notification("desktop-update");
        assert_eq!(ui.mode, Mode::Settings);
        assert_eq!(ui.settings_tab, SettingsTab::About);

        ui.start_desktop_update();
        ui.on_action(UiAction::CheckDesktopUpdateIfIdle);
        assert!(matches!(ui.desktop_update, Status::Installing { .. }));
        assert!(ui.desktop_update_panel);
    }

    #[test]
    fn update_card_pins_to_bottom_right_without_blocking_modal_scrim() {
        let (sender, _rx) = commands::channel();
        let mut s = UiState::new(sender);
        s.splash = None;
        s.receive_desktop_update(Status::Available("0.1.1-preview.99".into()));
        assert!(s.desktop_update_panel);

        let mut rt = rune::Runtime::new(
            s,
            crate::ui::view,
            (1440.0, 900.0),
            rune::Env::new(true, 1.0),
        );
        for _ in 0..8 {
            rt.frame(1.0 / 60.0);
        }
        let nodes = rt.elements();
        let card = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("desktop-update-card"))
            .expect("compact update card should mount");
        assert!(
            card.rect[0] > 900.0 && card.rect[1] > 600.0,
            "update card should sit in bottom-right corner, got {:?}",
            card.rect
        );
        assert!(
            nodes
                .iter()
                .all(|n| n.key.as_deref() != Some("download-panel-card")),
            "must not mount the old center modal download panel"
        );
    }
}
