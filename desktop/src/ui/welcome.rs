//! The first-run welcome: one card over the window the first time the
//! desktop opens with no session history. What Muqun Desktop is, the agents
//! it found and how each stands, and one press to start (or skip). Seen
//! once (`general.welcomed`); never again after a session exists.

use rune::{col, el, row, text, El, SurfaceFit, Transition};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use super::agents::{by_standing, Standing};
use super::copy::{tr, tr_ref};
use super::{Mode, UiState};
use crate::branding::{APP_MARK_PNG, APP_NAME};
use crate::config::ConfigEdit;
use crate::theme::look::PANEL_RADIUS;

impl UiState {
    /// Whether the welcome is due: never seen, discovery done, and nothing
    /// to come back to.
    pub fn show_welcome(&self) -> bool {
        !self.cfg.general.welcomed
            && self.splash.is_none()
            && self.agents_seen
            && !self.hub_label.is_empty()
            && self.sessions.is_empty()
            && self.cfg.general.last_agent.is_none()
    }

    fn dismiss_welcome(&mut self, start: bool) {
        self.edit(ConfigEdit::Welcomed);
        if start {
            self.go(Mode::Home);
            self.start_draft.focused = true;
        }
    }
}

/// The welcome card and its scrim, when due.
pub fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    if !s.show_welcome() {
        return;
    }
    let look = s.look();
    let language = s.cfg.general.language;
    let mut scrim = el()
        .key("welcome-scrim")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .fill(k.c.scrim)
        .a11y_hidden();
    if look.blur {
        scrim = scrim.backdrop_blur(12.0);
    }
    layer.push(scrim);

    // The badge: the app mark on a lit slab.
    let mut badge = el()
        .key("welcome-badge")
        .size(64.0, 64.0)
        .radius(18.0)
        .center()
        .fill(k.c.layer_02);
    if s.fx_on() {
        badge = badge.child(
            super::fx::metal("welcome-metal", 64.0, 18.0, k.c.bg_accent, 0.0).absolute(0.0, 0.0),
        );
    }
    if let Some(images) = s.images.as_ref() {
        badge = badge.child(
            super::art::picture(
                images,
                std::sync::Arc::<[u8]>::from(APP_MARK_PNG),
                SurfaceFit::Fill,
            )
            .size(38.0, 38.0)
            .radius(10.0)
            .clip_rounded(10.0),
        );
    }
    let badge = look.glow(badge, k.c.bg_accent, 0.7);

    let title_font = rune::TextStyle {
        size: 26.0,
        line_height: 32.0,
        ..k.ty.ui_medium
    };
    let mut agents = col().key("welcome-agents").w_full().gap(6.0);
    let ranked = by_standing(&s.agents);
    let usable = ranked
        .iter()
        .filter(|(_, st)| *st == Standing::Usable)
        .count();
    for (a, standing) in ranked.iter().take(8) {
        let (color, word) = match standing {
            Standing::Usable => (k.c.success.icon, standing.word(language)),
            Standing::Checking => (k.c.text_faint, standing.word(language)),
            Standing::Disabled | Standing::NotInstalled => {
                (k.c.text_faint, standing.word(language))
            }
            _ => (k.c.warning.icon, standing.word(language)),
        };
        agents = agents.child(
            row()
                .key(format!("welcome-agent-{}", a.id))
                .w_full()
                .h(36.0)
                .px(10.0)
                .gap(10.0)
                .center_y()
                .radius(8.0)
                .fill(k.c.layer_01.alpha(0.6))
                .child(super::agents::mark(&a.id, 16.0, s.dark).a11y_hidden())
                .child(
                    text(super::agents::name(&a.id, &s.agents))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .grow(1.0)
                        .truncate(),
                )
                .child(el().size(7.0, 7.0).radius(3.5).fill(color).a11y_hidden())
                .child(
                    text(word.to_string())
                        .font(k.ty.small)
                        .color(k.c.text_muted),
                ),
        );
    }
    let blurb = tr(
        language,
        "Every coding agent on this computer in one window: start sessions, \
         watch several at once, answer approvals, and keep the history. \
         Nothing leaves this machine unless an agent sends it.",
    );
    let agents_line = if usable == 0 {
        tr(
            language,
            "No agent is ready yet. Install one, or enable it in Settings → Agents.",
        )
    } else {
        super::copy::localize(
            language,
            tr(
                language,
                &format!(
                    "{usable} agent{} ready to go.",
                    if usable == 1 { "" } else { "s" }
                ),
            ),
        )
    };

    let card = col()
        .key("welcome-card")
        .w(520.0)
        .gap(18.0)
        .px(28.0)
        .py(28.0)
        .items_center()
        .role("dialog")
        .label(tr(language, "Welcome"))
        .child(badge)
        .child(
            col()
                .gap(6.0)
                .items_center()
                .child(
                    text(tr(language, "Welcome to Muqun"))
                        .font(title_font)
                        .color(k.c.text_base),
                )
                .child(
                    text(format!("{APP_NAME} Desktop"))
                        .font(k.ty.small_medium)
                        .color(k.c.text_accent),
                ),
        )
        .child(
            text(blurb)
                .font(k.ty.body)
                .color(k.c.text_muted)
                .wrap()
                .w_full()
                .align(rune::Align::Center),
        )
        .child(
            col()
                .w_full()
                .gap(8.0)
                .child(
                    row()
                        .w_full()
                        .center_y()
                        .child(
                            text(tr(language, "Agents found"))
                                .font(k.ty.body_medium)
                                .color(k.c.text_base)
                                .grow(1.0),
                        )
                        .child(text(agents_line).font(k.ty.small).color(k.c.text_muted)),
                )
                .child(agents),
        )
        .child(
            row()
                .key("welcome-actions")
                .gap(10.0)
                .center_y()
                .child(
                    Button::new(tr_ref(language, "Not now"))
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Large)
                        .on_press(|s: &mut UiState| s.dismiss_welcome(false))
                        .build(k)
                        .key("welcome-skip"),
                )
                .child(
                    look.glow(
                        Button::new(tr_ref(language, "Start a session"))
                            .variant(ButtonVariant::Submit)
                            .control_size(ControlSize::Large)
                            .on_press(|s: &mut UiState| s.dismiss_welcome(true))
                            .build(k)
                            .key("welcome-start"),
                        k.c.bg_accent,
                        0.6,
                    ),
                ),
        );
    let card = look
        .panel(k, card, PANEL_RADIUS)
        .modal(true, |s: &mut UiState| s.dismiss_welcome(false))
        .transition(
            Transition::scale(0.94)
                .combined(Transition::opacity())
                .combined(Transition::offset(0.0, 18.0)),
        )
        .motion(crate::theme::look::ARRIVE);
    let card = look.glow(card, k.c.bg_accent, 0.35);
    layer.push(
        el().key("welcome-layer")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .center()
            .child(card),
    );
}
