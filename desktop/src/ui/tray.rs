//! Keeps the status icon (`platform/tray.rs`) in step with what needs the
//! owner: approvals and questions waiting, by session.

use super::copy::tr;
use super::UiState;
use crate::platform::tray::Words;

/// The tray's words for `count` sessions waiting.
pub fn words(language: crate::config::Language, count: usize) -> Words {
    let waiting = (count > 0).then(|| {
        if count == 1 {
            tr(language, "1 session needs you", "1 个会话需要你")
        } else {
            tr(
                language,
                &format!("{count} sessions need you"),
                &format!("{count} 个会话需要你"),
            )
        }
    });
    Words {
        open: tr(language, "Open Muqun", "打开 Muqun"),
        quit: tr(language, "Quit Muqun", "退出 Muqun"),
        tooltip: match &waiting {
            Some(w) => format!("Muqun · {w}"),
            None => "Muqun".into(),
        },
        waiting,
    }
}

impl UiState {
    /// Refresh the tray when the set of waiting sessions changed.
    pub fn sync_tray(&mut self) {
        if !crate::platform::tray::active() {
            return;
        }
        let mut waiting: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter(|s| {
                self.waiting_perms
                    .get(&s.asid.0)
                    .is_some_and(|p| !p.is_empty())
                    || self
                        .waiting_forms
                        .get(&s.asid.0)
                        .is_some_and(|f| !f.is_empty())
            })
            .map(|s| (s.asid.0.clone(), super::title_of(s)))
            .collect();
        waiting.sort();
        let language = self.cfg.general.language;
        if self.tray_shown.as_ref() == Some(&(language, waiting.clone())) {
            return;
        }
        crate::platform::tray::update(&words(language, waiting.len()), &waiting);
        self.tray_shown = Some((language, waiting));
    }
}
