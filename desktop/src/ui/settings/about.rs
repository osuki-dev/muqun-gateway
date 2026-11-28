//! Settings → About: what this is, how it is connected, where to go next.

use std::path::PathBuf;
use std::sync::Arc;

use rune::{col, el, row as hrow, text, El, SurfaceFit};
use rune_kit::Kit;

use super::{button, group, link_button, row};
use crate::branding::{APP_MARK_PNG, APP_NAME};
use crate::config::Language;
use crate::platform::update::{self, GatewayStatus, Status};
use crate::ui::copy::{tr, tr_ref};
use crate::ui::UiState;

impl UiState {
    pub(in crate::ui) fn check_desktop_update(&mut self) {
        if !matches!(
            self.desktop_update,
            Status::Checking | Status::Ready { .. } | Status::Installing { .. }
        ) {
            self.desktop_update = Status::Checking;
            self.send(crate::model::commands::Command::CheckDesktopUpdate);
        }
    }

    pub(in crate::ui) fn check_gateway_update(&mut self) {
        if !matches!(
            self.gateway_update,
            GatewayStatus::Checking | GatewayStatus::Installing { .. }
        ) {
            self.gateway_update = GatewayStatus::Checking;
            self.send(crate::model::commands::Command::CheckGatewayUpdate);
        }
    }

    pub(in crate::ui) fn start_gateway_update(&mut self) {
        let (version, channel) = match &self.gateway_update {
            GatewayStatus::Available {
                latest,
                channel,
                can_install: true,
                ..
            } => (latest.clone(), channel.clone()),
            GatewayStatus::InstallFailed {
                version, channel, ..
            } => (version.clone(), channel.clone()),
            _ => return,
        };
        self.gateway_update = GatewayStatus::Installing {
            version: version.clone(),
        };
        self.send(crate::model::commands::Command::InstallGatewayUpdate { version, channel });
    }
}

fn hub_label(language: Language, raw: &str) -> String {
    match raw {
        "" => tr(language, "Connecting…"),
        "local gateway" => tr(language, "Local gateway"),
        "standalone" => tr(language, "Standalone"),
        other => other.to_string(),
    }
}

/// The app's mark, name, version and build, lit from behind.
fn hero(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let look = s.look();
    const MARK: f32 = 56.0;
    const MARK_RADIUS: f32 = 12.0;
    let mark = match s.images.as_ref() {
        Some(images) => {
            crate::ui::art::picture(images, Arc::<[u8]>::from(APP_MARK_PNG), SurfaceFit::Fill)
                .key("about-mark")
                .size(MARK, MARK)
                .radius(MARK_RADIUS)
                .clip_rounded(MARK_RADIUS)
        }
        None => el()
            .key("about-mark")
            .size(MARK, MARK)
            .radius(MARK_RADIUS)
            .fill(k.c.bg_accent),
    };
    let build = option_env!("MUQUN_BUILD").unwrap_or(if cfg!(debug_assertions) {
        "debug build"
    } else {
        "release build"
    });
    hrow()
        .key("about-hero")
        .w_full()
        .gap(18.0)
        .center_y()
        .px(20.0)
        .py(18.0)
        .radius(k.radius.lg)
        .fill(k.c.layer_01.alpha(0.65))
        .border(1.0, k.c.border_weak.alpha(0.5))
        .child(look.glow(mark, k.c.bg_accent, 0.5).a11y_hidden())
        .child(
            col()
                .key("about-words")
                .grow(1.0)
                .shrink()
                .gap(3.0)
                .child(
                    text(APP_NAME)
                        .font(rune::TextStyle {
                            size: 20.0,
                            line_height: 26.0,
                            ..k.ty.ui_medium
                        })
                        .color(k.c.text_base),
                )
                .child(
                    text(tr(
                        language,
                        "Coding agents on this computer, at your desk.",
                    ))
                    .font(k.ty.ui)
                    .color(k.c.text_muted)
                    .wrap(),
                )
                .child(
                    text(format!(
                        "{} {} · {build} · rune {}",
                        tr_ref(language, "Version"),
                        env!("CARGO_PKG_VERSION"),
                        crate::branding::RUNE_REV
                    ))
                    .key("about-version")
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .wrap(),
                ),
        )
}

fn license_row(k: &Kit, name: &str, license: &str, detail: &str) -> El {
    row(
        k,
        &format!("lic-{}", name.replace([' ', ','], "-")),
        name,
        detail,
        text(license.to_string())
            .font(k.ty.small_medium)
            .color(k.c.text_muted),
    )
}

fn path_row(k: &Kit, key: &str, title: &str, path: String) -> El {
    row(
        k,
        &format!("path-{key}"),
        title,
        "",
        hrow()
            .key("path")
            .w(super::control_w())
            .justify_end()
            .child(super::long_text(k, "path-text", path, k.ty.mono)),
    )
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let (config, state) =
        crate::platform::directories().unwrap_or_else(|_| (PathBuf::new(), PathBuf::new()));
    let live = !s.hub_label.is_empty();
    let hub = hrow()
        .key("hub")
        .gap(8.0)
        .center_y()
        .child(s.look().spark(
            el().size(7.0, 7.0).radius(3.5).fill(if live {
                k.c.success.icon
            } else {
                k.c.text_faint
            }),
            if live {
                k.c.success.icon
            } else {
                k.c.text_faint
            },
        ))
        .child(
            text(hub_label(language, &s.hub_label))
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        );
    let theme_name = crate::theme::apply::current_title()
        .map(|(name, _)| name)
        .unwrap_or_else(|| s.cfg.appearance.theme_id.clone());
    let repo = env!("CARGO_PKG_REPOSITORY");
    let channel = tr_ref(
        language,
        if update::is_preview_build() {
            "Preview releases"
        } else {
            "Stable releases"
        },
    );
    let result = match &s.desktop_update {
        Status::Idle => String::new(),
        Status::Checking => tr(language, "Checking…"),
        Status::Current => tr(language, "Up to date"),
        Status::NoRelease => tr(language, "No published version in this channel"),
        Status::Available(version) | Status::Manual(version) => {
            format!("{} {version}", tr_ref(language, "New version available:"))
        }
        Status::Failed => tr(language, "Check failed"),
        Status::InstallFailed(_) => tr(language, "Desktop update failed"),
        Status::Installing { phase, .. } => {
            tr(language, crate::ui::desktop_update::phase_label(*phase))
        }
        Status::Ready { version, .. } => format!(
            "{} · {version}",
            tr_ref(language, "Restart to use the installed update.")
        ),
    };
    let update_row = row(
        k,
        "update-manual",
        channel,
        &result,
        hrow()
            .key("desktop-update-actions")
            .gap(6.0)
            .center_y()
            .flex_wrap()
            .child(
                match &s.desktop_update {
                    Status::Available(_) | Status::InstallFailed(_) => button(tr_ref(
                        language,
                        if matches!(s.desktop_update, Status::InstallFailed(_)) {
                            "Retry"
                        } else {
                            "Download & install"
                        },
                    ))
                    .on_press(UiState::start_desktop_update),
                    Status::Installing { .. } | Status::Ready { .. } => {
                        button(tr_ref(language, "Details")).on_press(UiState::show_desktop_update)
                    }
                    other => button(tr_ref(
                        language,
                        if matches!(other, Status::Idle | Status::Checking) {
                            "Check for updates"
                        } else {
                            "Check again"
                        },
                    ))
                    .disabled(*other == Status::Checking)
                    .on_press(UiState::check_desktop_update),
                }
                .build(k)
                .key("desktop-update-action"),
            )
            .child(link_button(
                k,
                "desktop-downloads",
                tr_ref(language, "View downloads"),
                update::INSTALL_URL,
            )),
    );
    let current_computer = s.current_computer();
    let is_remote = !matches!(current_computer, "local" | "self" | "all" | "");
    let gateway_title = if is_remote {
        format!("Muqun Gateway · {}", s.computer_name(current_computer))
    } else {
        "Muqun Gateway".to_string()
    };
    let gateway_result = match &s.gateway_update {
        GatewayStatus::Idle => String::new(),
        GatewayStatus::Checking => tr(language, "Checking…"),
        GatewayStatus::Standalone => tr(language, "Standalone mode (embedded)"),
        GatewayStatus::Current { installed } => {
            format!("v{installed} · {}", tr_ref(language, "Up to date"))
        }
        GatewayStatus::Available {
            installed, latest, ..
        } => format!("v{installed} → {latest} {}", tr_ref(language, "available")),
        GatewayStatus::Manual {
            installed,
            latest,
            remote,
            ..
        } => {
            let note = if *remote {
                tr_ref(language, "Remote host — update on that computer")
            } else {
                tr_ref(language, "Update manually via installer")
            };
            format!("v{installed} → {latest} · {note}")
        }
        GatewayStatus::NoRelease { installed } => format!(
            "v{installed} · {}",
            tr_ref(language, "No published version in this channel")
        ),
        GatewayStatus::Installing { .. } => tr(language, "Updating Gateway…"),
        GatewayStatus::Updated { version } => {
            format!("{} {version}", tr_ref(language, "Updated to"))
        }
        GatewayStatus::InstallFailed { error, .. } => {
            if error.is_empty() {
                tr(language, "Gateway update failed")
            } else {
                format!("{}: {error}", tr_ref(language, "Gateway update failed"))
            }
        }
        GatewayStatus::Failed(error) => {
            if error.is_empty() {
                tr(language, "Check failed")
            } else {
                format!("{}: {error}", tr_ref(language, "Check failed"))
            }
        }
    };
    let gateway_row = row(
        k,
        "update-gateway",
        &gateway_title,
        &gateway_result,
        hrow()
            .key("gateway-update-actions")
            .gap(6.0)
            .center_y()
            .flex_wrap()
            .child(
                match &s.gateway_update {
                    GatewayStatus::Available {
                        can_install: true, ..
                    } => button(tr_ref(language, "Update Gateway"))
                        .on_press(UiState::start_gateway_update),
                    GatewayStatus::InstallFailed { .. } => {
                        button(tr_ref(language, "Retry")).on_press(UiState::start_gateway_update)
                    }
                    GatewayStatus::Installing { .. } => {
                        button(tr_ref(language, "Updating…")).disabled(true)
                    }
                    other => button(tr_ref(
                        language,
                        if matches!(other, GatewayStatus::Idle | GatewayStatus::Checking) {
                            "Check for updates"
                        } else {
                            "Check again"
                        },
                    ))
                    .disabled(*other == GatewayStatus::Checking)
                    .on_press(UiState::check_gateway_update),
                }
                .build(k)
                .key("gateway-update-action"),
            )
            .child(link_button(
                k,
                "gateway-downloads",
                tr_ref(language, "View downloads"),
                update::GATEWAY_INSTALL_URL,
            )),
    );
    vec![
        hero(s, k),
        group(
            k,
            "about-updates",
            tr_ref(language, "Updates"),
            vec![update_row, gateway_row],
        ),
        group(
            k,
            "about-status",
            tr_ref(language, "This window"),
            vec![
                row(
                    k,
                    "hub-mode",
                    tr_ref(language, "Connection"),
                    match s.hub_label.as_str() {
                        "local gateway" => tr_ref(
                            language,
                            "Sessions come from the Muqun gateway on this computer; your phone sees the same ones."
                        ),
                        "standalone" => tr_ref(
                            language,
                            "No gateway is running; this window talks to the agents directly."
                        ),
                        _ => tr_ref(language, "How this window reaches the agents."),
                    },
                    hub,
                ),
                row(
                    k,
                    "theme-about",
                    tr_ref(language, "Theme"),
                    "",
                    text(theme_name).font(k.ty.ui).color(k.c.text_base),
                ),
            ],
        ),
        group(
            k,
            "about-links",
            tr_ref(language, "Links"),
            vec![
                row(
                    k,
                    "link-site",
                    tr_ref(language, "Website"),
                    "muqun.dev",
                    link_button(k, "about-site", tr_ref(language, "Open"), "https://muqun.dev"),
                ),
                row(
                    k,
                    "link-themes",
                    tr_ref(language, "Theme gallery"),
                    "muqun.dev/themes",
                    link_button(
                        k,
                        "about-themes",
                        tr_ref(language, "Open"),
                        "https://muqun.dev/themes",
                    ),
                ),
                row(
                    k,
                    "link-repo",
                    tr_ref(language, "Source code"),
                    repo.trim_start_matches("https://"),
                    link_button(k, "about-repo", tr_ref(language, "Open"), repo),
                ),
                row(
                    k,
                    "link-issues",
                    tr_ref(language, "Report an issue"),
                    tr_ref(
                        language,
                        "Tell us what went wrong, or what you'd like."
                    ),
                    link_button(
                        k,
                        "about-issues",
                        tr_ref(language, "Open"),
                        concat!(env!("CARGO_PKG_REPOSITORY"), "/issues"),
                    ),
                ),
            ],
        ),
        group(
            k,
            "about-licenses",
            tr_ref(language, "Open source"),
            vec![
                license_row(
                    k,
                    "muqun-desktop",
                    "Apache-2.0",
                    tr_ref(
                        language,
                        "This app, and the gateway it embeds."
                    ),
                ),
                license_row(
                    k,
                    "Rune, rune-kit, rune-fx",
                    "MIT",
                    tr_ref(
                        language,
                        "The UI framework, components and GPU effects."
                    ),
                ),
                license_row(
                    k,
                    "OpenCC data",
                    "Apache-2.0",
                    tr_ref(
                        language,
                        "The Simplified to Traditional Chinese table."
                    ),
                ),
                license_row(
                    k,
                    tr_ref(language, "Built-in themes"),
                    "CC0-1.0",
                    tr_ref(
                        language,
                        "Six theme packs and their artwork."
                    ),
                ),
                license_row(
                    k,
                    "JetBrains Mono, Noto Sans CJK",
                    "OFL-1.1",
                    tr_ref(
                        language,
                        "Fonts the UI asks the system for; not bundled."
                    ),
                ),
            ],
        ),
        group(
            k,
            "about-paths",
            tr_ref(language, "Files"),
            vec![
                path_row(
                    k,
                    "config",
                    tr_ref(language, "Settings"),
                    config.display().to_string(),
                ),
                path_row(
                    k,
                    "state",
                    tr_ref(language, "Sessions and cache"),
                    state.display().to_string(),
                ),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::commands::{self, Command},
        platform::installer::Phase,
    };

    #[test]
    fn about_download_can_be_backgrounded_and_ready_never_restarts_by_itself() {
        fn view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let kit = Kit::neutral(true);
            let mut layer = Vec::new();
            crate::ui::desktop_update::mount(s, &kit, &mut layer);
            rune_kit::root(col().w_full().children(page(s, &kit)), layer)
        }
        let (sender, mut receiver) = commands::channel();
        let mut s = UiState::new(sender);
        s.splash = None;
        s.desktop_update = Status::Available("1.2.0".into());
        while receiver.try_recv().is_ok() {} // Ignore startup discovery and keychain reads.
        let mut rt = rune::Runtime::new(s, view, (1100.0, 900.0), rune::Env::new(true, 1.0));
        let mut time = 0.0;
        let mut click = |rt: &mut rune::Runtime<UiState>, key: &str| {
            for _ in 0..25 {
                time += 1.0 / 60.0;
                rt.frame(time);
            }
            let (x, y) = rt.find(key).unwrap();
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            time += 1.0 / 60.0;
            rt.frame(time);
        };
        click(&mut rt, "desktop-update-action");
        assert!(
            matches!(receiver.try_recv(),Ok(Command::InstallDesktopUpdate(version)) if version=="1.2.0")
        );
        rt.state.desktop_update = Status::Installing {
            version: "1.2.0".into(),
            phase: Some(Phase::Downloading {
                received: 12,
                total: 20,
            }),
        };
        click(&mut rt, "desktop-update-hide");
        assert!(!rt.state.desktop_update_panel);
        click(&mut rt, "desktop-update-action");
        assert!(rt.state.desktop_update_panel);
        rt.state.desktop_update = Status::Ready {
            version: "1.2.0".into(),
            executable: "/fixture/unused".into(),
        };
        click(&mut rt, "desktop-update-hide");
        assert!(receiver.try_recv().is_err());
        assert!(!rt.state.restart_on_frame);
    }

    #[test]
    fn about_gateway_update_row_checks_and_installs() {
        fn view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let kit = Kit::neutral(true);
            rune_kit::root(col().w_full().children(page(s, &kit)), Vec::new())
        }
        let (sender, mut receiver) = commands::channel();
        let mut s = UiState::new(sender);
        s.splash = None;
        while receiver.try_recv().is_ok() {}
        let mut rt = rune::Runtime::new(s, view, (1100.0, 900.0), rune::Env::new(true, 1.0));
        let mut time = 0.0;
        let mut click = |rt: &mut rune::Runtime<UiState>, key: &str| {
            for _ in 0..25 {
                time += 1.0 / 60.0;
                rt.frame(time);
            }
            let (x, y) = rt.find(key).unwrap();
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            time += 1.0 / 60.0;
            rt.frame(time);
        };

        // Idle -> clicking dispatches Command::CheckGatewayUpdate
        click(&mut rt, "gateway-update-action");
        assert_eq!(rt.state.gateway_update, GatewayStatus::Checking);
        assert!(matches!(
            receiver.try_recv(),
            Ok(Command::CheckGatewayUpdate)
        ));

        // Available (local) -> clicking dispatches Command::InstallGatewayUpdate
        rt.update(|s, _| {
            s.apply_updates(vec![crate::model::inbox::Update::GatewayUpdate(
                GatewayStatus::Available {
                    installed: "0.14.1-preview.34".into(),
                    latest: "0.14.1-preview.35".into(),
                    channel: "preview".into(),
                    can_install: true,
                },
            )]);
        });
        click(&mut rt, "gateway-update-action");
        assert_eq!(
            rt.state.gateway_update,
            GatewayStatus::Installing {
                version: "0.14.1-preview.35".into(),
            }
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(Command::InstallGatewayUpdate { version, channel })
                if version == "0.14.1-preview.35" && channel == "preview"
        ));

        // InstallFailed -> shows toast and clicking Retry dispatches Command::InstallGatewayUpdate again
        rt.update(|s, _| {
            s.apply_updates(vec![crate::model::inbox::Update::GatewayUpdate(
                GatewayStatus::InstallFailed {
                    version: "0.14.1-preview.35".into(),
                    channel: "preview".into(),
                    error: "network error".into(),
                },
            )]);
        });
        assert!(!rt.state.toasts.is_empty());
        click(&mut rt, "gateway-update-action");
        assert!(matches!(
            receiver.try_recv(),
            Ok(Command::InstallGatewayUpdate { version, channel })
                if version == "0.14.1-preview.35" && channel == "preview"
        ));

        // Updated -> records updated status and emits toast
        rt.state.toasts.clear();
        rt.update(|s, _| {
            s.apply_updates(vec![crate::model::inbox::Update::GatewayUpdate(
                GatewayStatus::Updated {
                    version: "0.14.1-preview.35".into(),
                },
            )]);
        });
        assert_eq!(
            rt.state.gateway_update,
            GatewayStatus::Updated {
                version: "0.14.1-preview.35".into(),
            }
        );
        assert!(!rt.state.toasts.is_empty());
    }
}
