//! A QA script for the composer, driven by an environment variable so a
//! headed run needs no synthetic input (no focus to steal, no pointer moved
//! on a shared desktop). Off unless `MUQUN_QA_COMPOSER` is set.
//!
//! `MUQUN_QA_COMPOSER=<step>;<step>;…`, one step every
//! `MUQUN_QA_COMPOSER_EVERY` seconds (default 3), starting after the same
//! pause. Steps:
//!
//! * `text:<s>` -- the draft becomes `<s>` with the caret at its end, as
//!   typing it would (the popup follows).
//! * `menu:<project|agent|model|mode|effort|branch|usage|yolo>` -- open that pill's
//!   menu as a click would; `menu:close` closes it.
//! * `key:<up|down|enter|tab>` -- the key reaches the composer.
//! * `esc` -- Esc as the window handles it.
//! * `attach:<path>` -- stage that file as the dialog would.
//! * `paste:<path>` -- stage that PNG as a pasted image.
//! * `submit` -- send as Enter does.
//! * `scheme:<light|dark>` -- switch the colour scheme.
//! * `agent:<id>` -- choose the start page's agent.
//! * `yolo:<on|off>[:asid]` -- switch YOLO on the open (or named) session, as the chip does
//!   after its confirmation.
//! * `drop:<path>` -- drop that file or folder on the window.

use std::time::Duration;

use super::state::MenuId;
use super::{pills, triggers};
use crate::config::Scheme;
use crate::model::inbox::UiAction;
use crate::ui::App;

pub fn start(app: &App) {
    let Ok(script) = std::env::var("MUQUN_QA_COMPOSER") else {
        return;
    };
    let every = std::env::var("MUQUN_QA_COMPOSER_EVERY")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(3.0);
    let drain = app.drain.clone();
    std::thread::spawn(move || {
        for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            std::thread::sleep(Duration::from_secs_f32(every));
            tracing::info!(step, "qa: composer step");
            drain.post(UiAction::QaStep(step.to_string()));
        }
    });
}

/// Run one step, on the main thread.
pub fn run(app: &App, step: &str) {
    let (name, arg) = step.split_once(':').unwrap_or((step, ""));
    match name {
        "text" => {
            let end = arg.chars().count();
            super::set_text(app, arg, end);
            super::edited(app);
        }
        "menu" => match arg {
            "close" => pills::close_menu(),
            other => {
                let id = match other {
                    "project" => MenuId::Project,
                    "agent" => MenuId::Agent,
                    "model" => MenuId::Model,
                    "mode" => MenuId::Mode,
                    "effort" => MenuId::Effort,
                    "branch" => MenuId::Branch,
                    "usage" => MenuId::Usage,
                    "yolo" => MenuId::Yolo,
                    _ => return,
                };
                if !pills::open_by_id(id) {
                    tracing::warn!(arg, "qa: no such pill on screen");
                }
            }
        },
        "bounds" => {
            let b = blinc_layout::selector::query(arg).and_then(|h| h.bounds());
            tracing::info!(id = arg, ?b, "qa: bounds");
        }
        "key" => {
            let code = match arg {
                "up" => 38,
                "down" => 40,
                "enter" => 13,
                "tab" => 9,
                _ => return,
            };
            let (text, caret) = super::current_text(app);
            let (line, column) = triggers::caret_position(&text, caret);
            if code == 13 {
                // The text area would have inserted a newline first.
                super::set_text(app, &format!("{text}\n"), caret + 1);
            }
            super::on_key(app, code, line, column);
        }
        "esc" => {
            super::dismiss();
        }
        "attach" => super::files_picked(app, vec![arg.into()]),
        "paste" => {
            if let Ok(png) = std::fs::read(arg) {
                super::image_pasted(app, png);
            }
        }
        "submit" => super::submit(app),
        "scheme" => app.apply_scheme(match arg {
            "light" => Scheme::Light,
            _ => Scheme::Dark,
        }),
        "agent" => {
            let cmp = super::state::Cmp::get();
            let mut choice = (*cmp.start.get()).clone();
            choice.agent = Some(arg.to_string());
            choice.model = None;
            choice.mode = None;
            cmp.start.signal().set(std::sync::Arc::new(choice));
        }
        "yolo" => {
            let (state, named) = arg.split_once(':').unwrap_or((arg, ""));
            let asid = if named.is_empty() {
                super::aim(app, super::Place::Session).asid()
            } else {
                Some(muqun_gateway::agents::AgentSessionId(named.to_string()))
            };
            if let Some(asid) = asid {
                pills::switch_yolo(app, &asid, state == "on");
            }
        }
        "drop" => super::route_drop(&app.drain, &app.commands, vec![arg.into()]),
        other => tracing::warn!(other, "qa: unknown composer step"),
    }
}
