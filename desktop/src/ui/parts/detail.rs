use muqun_gateway::agents::ports::AgentSessionSnapshot;
use muqun_gateway::agents::AgentSessionId;
use rune::{col, row, text, El};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use crate::config::Language;
use crate::plugin::{DesktopPlugin, PluginViewContext};
pub(crate) const ID: &str = "subagent-detail";

pub(crate) struct SubagentWindow;

impl DesktopPlugin for SubagentWindow {
    fn id(&self) -> &'static str {
        ID
    }
    fn title(&self, lang: Language) -> String {
        tr_ref(lang, "Subagent").to_owned()
    }
    fn icon(&self) -> &'static rune::Icon {
        &crate::ui::icons::AGENT
    }
    fn window_title(&self, s: &UiState) -> Option<String> {
        let asid = s.detail.as_ref()?.shown()?;
        Some(title(s, asid))
    }
    fn window_actions(&self, s: &UiState, k: &Kit) -> Option<El> {
        let detail = s.detail.as_ref()?;
        Some(actions(s, k, detail, detail.shown()?))
    }
    fn can_move_view(&self, _: &str) -> bool {
        true
    }
    fn render_tile(&self, ctx: &PluginViewContext<'_>, _: &mut Vec<El>) -> Option<El> {
        body(ctx.s, ctx.k)
    }
}

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
    pub(crate) fn open_detail(&mut self, parent: &str, asid: &str) {
        self.remember_subagent_context(asid, Some(parent));
        let sheet = self.detail.get_or_insert_with(DetailSheet::default);
        if sheet.shown() != Some(asid) {
            if sheet.shown() != Some(parent) {
                sheet.stack.clear();
            }
            if let Some(index) = sheet.stack.iter().position(|id| id == asid) {
                sheet.stack.truncate(index + 1);
            } else {
                sheet.stack.push(asid.to_owned());
            }
        }
        self.refresh_detail(asid);
        if self.active_plugin_page.as_deref() == Some(ID) {
            return;
        }
        if let Some(i) = self.tile_of_plugin(ID) {
            self.focus_tile(i);
        } else {
            self.open_plugin_drawer(ID);
        }
        self.retain_timelines();
    }

    fn refresh_detail(&mut self, asid: &str) {
        if let Some(detail) = &mut self.detail {
            detail.error = None;
        }
        self.send(Command::ReadDetail(AgentSessionId(asid.to_owned())));
    }

    fn detail_back(&mut self) {
        if let Some(detail) = &mut self.detail {
            detail.stack.pop();
            if let Some(asid) = detail.shown().map(str::to_owned) {
                self.refresh_detail(&asid);
            }
        }
        self.retain_timelines();
    }

    pub(crate) fn close_detail(&mut self) {
        self.close_plugin_windows(ID);
        self.detail = None;
        self.retain_timelines();
    }

    pub(crate) fn detail_window_focused(&self) -> bool {
        self.detail.is_some()
            && (self.active_plugin_page.as_deref() == Some(ID)
                || self.tile_of_plugin(ID) == Some(self.focus) && self.active_plugin_page.is_none())
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
                self.remember_timeline_lineage(&snapshot.info.asid.0, &snapshot.timeline);
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

fn title(s: &UiState, asid: &str) -> String {
    if let Some(run) = s
        .orchestration
        .runs
        .iter()
        .rev()
        .find(|r| r.reviewer_asid.as_deref() == Some(asid))
    {
        return format!(
            "{} · {}",
            tr_ref(s.cfg.general.language, "Reviewer"),
            s.agent_name(&run.reviewer.agent)
        );
    }
    let info = s.session_by_id(asid);
    let raw = info
        .and_then(|i| {
            i.agent_name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .or_else(|| {
                    (!i.title.trim().is_empty() && !i.title.eq_ignore_ascii_case("subagent"))
                        .then_some(i.title.as_str())
                })
        })
        .unwrap_or_else(|| tr_ref(s.cfg.general.language, "Subagent"));
    let first = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(raw);
    let cleaned = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() > 64 {
        let head: String = cleaned.chars().take(63).collect();
        format!("{head}…")
    } else if cleaned.is_empty() {
        tr_ref(s.cfg.general.language, "Subagent").to_owned()
    } else {
        cleaned
    }
}

fn actions(s: &UiState, k: &Kit, sheet: &DetailSheet, asid: &str) -> El {
    let t = |en| tr_ref(s.cfg.general.language, en);
    let mut bar = row().center_y().gap(4.0);
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
    let (refresh, open) = (asid.to_string(), asid.to_string());
    bar.child(
        crate::ui::icons::header_button(&crate::ui::icons::REFRESH, t("Refresh"))
            .on_press(move |s: &mut UiState| s.refresh_detail(&refresh))
            .build(k)
            .key("detail-refresh"),
    )
    .child(
        crate::ui::icons::header_button(&crate::ui::icons::OPEN_EXTERNAL, t("Open session"))
            .build(k)
            .key("detail-open")
            .on_press_tx(move |s: &mut UiState, tx: &mut rune::Tx| {
                let mods = tx.mods();
                s.close_detail();
                if mods.ctrl || mods.cmd || mods.alt || mods.shift {
                    s.open_in_tile(&open);
                } else {
                    s.open_session(&open);
                }
            }),
    )
}

fn body(s: &UiState, k: &Kit) -> Option<El> {
    let sheet = s.detail.as_ref()?;
    let asid = sheet.shown()?;
    let language = s.cfg.general.language;
    let timeline = s.timelines.get(asid);
    let has_echoes = s.timeline_echoes(asid).next().is_some();
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
                    .on_press(move |s: &mut UiState| s.refresh_detail(&retry))
                    .build(k),
            )
    } else if timeline.is_some_and(|tl| !tl.items.is_empty()) || has_echoes {
        session_view(s, k, asid)
    } else if timeline.is_some() {
        let busy = s.session_busy(asid);
        col()
            .key("detail-empty")
            .w_full()
            .grow(1.0)
            .center()
            .gap(10.0)
            .children(busy.then(|| crate::ui::art::ActivityMark::Standalone.build(k)))
            .child(
                text(tr_ref(
                    language,
                    if busy {
                        "Working…"
                    } else {
                        "No messages yet."
                    },
                ))
                .font(k.ty.ui)
                .color(k.c.text_base),
            )
    } else {
        col()
            .key("detail-loading")
            .w_full()
            .grow(1.0)
            .center()
            .child(crate::ui::art::ActivityMark::Standalone.build(k))
    };
    let mut facts = Vec::new();
    if let Some(info) = s.session_by_id(asid) {
        if let Some(role) = info.agent_role.as_deref().filter(|r| !r.is_empty()) {
            facts.push(role.to_owned());
        }
        if let Some(model) = &info.model {
            facts.push(crate::ui::labels::format_model_name(
                model,
                &model.model_id,
                None,
            ));
        }
        if let Some(tokens) = info
            .context_usage
            .as_ref()
            .map(|c| c.total_tokens)
            .filter(|t| *t > 0)
        {
            facts.push(super::context::tokens_label(language, tokens));
        }
    }
    Some(
        col()
            .key(format!("detail-panel-{asid}"))
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .children((!facts.is_empty()).then(|| {
                text(facts.join(" · "))
                    .px(12.0)
                    .py(4.0)
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .single_line()
                    .truncate()
            }))
            .child(body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PluginSlot;

    #[test]
    fn siblings_nested_targets_and_placement_keep_the_correct_conversation() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.mode = crate::ui::Mode::Session;
        s.selected = Some("parent".into());
        s.open_plugin_drawer("quicknotes");
        s.open_detail("parent", "first");
        assert_eq!(s.selected_dock_tab(), Some(ID));
        s.open_detail("parent", "second");
        s.on_detail("first".into(), Err("stale response".into()));
        assert_eq!(s.detail_asids(), ["second"]);
        assert!(s.detail.as_ref().unwrap().error.is_none());
        s.open_detail("second", "nested");
        assert_eq!(s.detail_asids(), ["second", "nested"]);
        s.detail_back();
        assert_eq!(s.detail_asids(), ["second"]);
        s.switch_plugin_surface(ID, PluginSlot::Page);
        assert_eq!(s.active_plugin_page.as_deref(), Some(ID));
        assert_eq!(s.detail_asids(), ["second"]);
        s.close_plugin_page();
        assert_eq!(s.selected_dock_tab(), Some(ID));
        s.switch_plugin_surface(ID, PluginSlot::Tile);
        let i = s.tile_of_plugin(ID).unwrap();
        s.toggle_tile_pop_at(i);
        assert!(s.tiles[i].popped);
        assert_eq!(s.detail_asids(), ["second"]);
        s.close_tile(i);
        assert!(s.detail.is_none());
        assert_eq!(s.selected.as_deref(), Some("parent"));
        s.open_plugin_drawer("quicknotes");
        s.open_detail("parent", "third");
        s.close_plugin_drawer();
        assert_eq!(s.selected_dock_tab(), Some("quicknotes"));
        assert!(s.detail.is_none());
    }

    #[test]
    fn detail_window_title_sanitizes_multiline_prompt_and_labels_mao_reviewer() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.sessions.push(
            serde_json::from_value(serde_json::json!({
                "asid": "rev-multiline",
                "agent_id": "codex",
                "title": "You are the designated **Read-Only Code Reviewer**.\n\nInspect the diff.\n\nReturn MAO_VERDICT.",
                "status": "busy",
                "updated_ms": 1
            }))
            .unwrap(),
        );
        let sanitized = title(&s, "rev-multiline");
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\r'));
        assert!(sanitized.starts_with("You are the designated"));

        s.orchestration.runs.push(crate::ui::orchestration::MaoRun {
            id: "mao-1".into(),
            reviewer: crate::ui::orchestration::MaoRole {
                agent: "codex".into(),
                model: None,
                machine: None,
            },
            reviewer_asid: Some("rev-multiline".into()),
            ..Default::default()
        });
        let labeled = title(&s, "rev-multiline");
        assert!(!labeled.contains('\n'));
        assert!(labeled.contains("codex") || labeled.contains("Codex"));
    }
}
