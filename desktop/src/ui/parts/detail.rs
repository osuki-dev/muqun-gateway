//! A subagent's session in a sheet over its parent (App
//! `agent-subagent-detail-sheet`): a read-only snapshot of the child's
//! timeline, drawn with the same cards, with Refresh, Open session, and
//! Back through nested subagents to the parent.

use muqun_gateway::agents::ports::AgentSessionSnapshot;
use muqun_gateway::agents::AgentSessionId;
use rune::{col, icon, row, text, Cursor, El};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Spinner};

use super::glyphs;
use crate::model::commands::Command;
use crate::ui::copy::tr_ref;
use crate::ui::timeline::session_view;
use crate::ui::UiState;

/// Which subagent sessions the sheet has walked into, the shown one last.
#[derive(Debug, Default)]
pub struct DetailSheet {
    pub stack: Vec<String>,
    /// The shown session could not be read.
    pub error: Option<String>,
}

impl DetailSheet {
    pub fn shown(&self) -> Option<&str> {
        self.stack.last().map(String::as_str)
    }
}

impl UiState {
    /// Show `asid` (a subagent's session) in the sheet, over whatever it
    /// shows now; Back returns there.
    pub fn open_detail(&mut self, asid: &str) {
        let sheet = self.detail.get_or_insert_with(DetailSheet::default);
        if sheet.shown() != Some(asid) {
            sheet.stack.push(asid.to_string());
        }
        sheet.error = None;
        self.send(Command::ReadDetail(AgentSessionId(asid.to_string())));
    }

    /// One step back toward the parent; the last step closes the sheet.
    pub fn detail_back(&mut self) {
        if let Some(sheet) = self.detail.as_mut() {
            sheet.stack.pop();
            sheet.error = None;
            if sheet.stack.is_empty() {
                self.detail = None;
            }
        }
        self.retain_timelines();
    }

    pub fn close_detail(&mut self) {
        self.detail = None;
        self.retain_timelines();
    }

    /// The core's read of a sheet's session.
    pub(in crate::ui) fn on_detail(
        &mut self,
        asid: String,
        result: Result<Box<AgentSessionSnapshot>, String>,
    ) {
        let Some(sheet) = self.detail.as_mut() else {
            return;
        };
        if sheet.shown() != Some(asid.as_str()) {
            return;
        }
        match result {
            Ok(snapshot) => {
                sheet.error = None;
                let snapshot = *snapshot;
                self.merge_sessions(vec![snapshot.info.clone()]);
                self.timelines
                    .entry(asid)
                    .or_default()
                    .replace(snapshot.timeline);
            }
            Err(err) => sheet.error = Some(err),
        }
    }

    /// The sessions the sheet keeps timelines for.
    pub(in crate::ui) fn detail_asids(&self) -> Vec<String> {
        self.detail
            .as_ref()
            .map(|d| d.stack.clone())
            .unwrap_or_default()
    }
}

fn header(s: &UiState, k: &Kit, sheet: &DetailSheet, asid: &str) -> El {
    let language = s.cfg.general.language;
    let t = |en| tr_ref(language, en);
    let info = s.session_by_id(asid);
    let title = info
        .map(|i| i.title.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| t("Subagent").to_string());
    let mut facts = vec![t("Subagent").to_string()];
    if let Some(i) = info {
        if let Some(role) = i.agent_role.as_ref().or(i.agent_name.as_ref()) {
            facts.push(role.clone());
        }
        if let Some(model) = &i.model {
            facts.push(crate::ui::labels::format_model_name(
                model,
                &model.model_id,
                None,
            ));
        }
        if let Some(tokens) = i
            .context_usage
            .as_ref()
            .map(|c| c.total_tokens)
            .filter(|t| *t > 0)
        {
            facts.push(super::context::tokens_label(language, tokens));
        }
    }
    let mut bar = row()
        .key("detail-head")
        .w_full()
        .gap(8.0)
        .px(14.0)
        .py(10.0)
        .center_y();
    if sheet.stack.len() > 1 {
        bar = bar.child(
            Button::new(t("Back"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.detail_back())
                .build(k)
                .key("detail-back"),
        );
    }
    bar = bar.child(
        col()
            .gap(1.0)
            .shrink()
            .grow(1.0)
            .child(
                text(title)
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .single_line()
                    .truncate(),
            )
            .child(
                text(facts.join(" · "))
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .single_line()
                    .truncate(),
            ),
    );
    let (refresh, open) = (asid.to_string(), asid.to_string());
    bar.child(
        Button::new(t("Refresh"))
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| s.open_detail(&refresh))
            .build(k)
            .key("detail-refresh"),
    )
    .child(
        Button::new(t("Open session"))
            .variant(ButtonVariant::Outline)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| {
                s.close_detail();
                s.open_session(&open);
            })
            .build(k)
            .key("detail-open"),
    )
    .child(
        k.interactive(
            row()
                .key("detail-close")
                .size(28.0, 28.0)
                .center()
                .radius(k.radius.md)
                .child(icon(&glyphs::CLOSE, 14.0).color(k.c.icon_muted)),
        )
        .cursor(Cursor::Pointer)
        .label(t("Close"))
        .on_press(|s: &mut UiState| s.close_detail()),
    )
}

/// The sheet over the window's right side, when one is open.
pub fn sheet(s: &UiState, k: &Kit) -> Option<El> {
    let sheet = s.detail.as_ref()?;
    let asid = sheet.shown()?;
    let language = s.cfg.general.language;
    let body = if let Some(err) = &sheet.error {
        let retry = asid.to_string();
        col()
            .key("detail-error")
            .w_full()
            .grow(1.0)
            .center()
            .gap(10.0)
            .child(
                text(tr_ref(language, "Could not load the session"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
            )
            .child(crate::ui::muted(k, err.clone()))
            .child(
                Button::new(tr_ref(language, "Try again"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| s.open_detail(&retry))
                    .build(k),
            )
    } else if s.timelines.contains_key(asid) {
        session_view(s, k, asid)
    } else {
        col()
            .key("detail-loading")
            .w_full()
            .grow(1.0)
            .center()
            .child(Spinner::new().build(k))
    };
    let panel = col()
        .key(format!("detail-panel-{asid}"))
        .w_frac(0.56)
        .min_w(420.0)
        .max_w(760.0)
        .h_full()
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_weak)
        .on_press(|_: &mut UiState| {})
        .child(header(s, k, sheet, asid))
        .child(col().h(1.0).w_full().fill(k.c.border_weak))
        .child(body);
    let panel = k.elevate(panel, &k.elevation.overlay);
    let panel = if s.cfg.appearance.reduced_motion {
        panel.transition(rune::Transition::opacity())
    } else {
        panel.transition(rune::Transition::offset(48.0, 0.0))
    };
    Some(
        row()
            .key("timeline-detail")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .justify_end()
            .fill(k.alpha(k.c.black, 0.28))
            .on_press(|s: &mut UiState| s.close_detail())
            .modal(true, |s: &mut UiState| s.close_detail())
            .child(panel),
    )
}
