use crate::{
    diagnostics::{self, Event},
    ui::UiState,
};
use rune::El;
use rune_kit::Kit;

#[derive(Default)]
pub struct DiagnosticFilter {
    pub selected: Option<uuid::Uuid>,
    source: Option<String>,
    all_outcomes: bool,
    last_hour: bool,
    page: usize,
}

impl DiagnosticFilter {
    fn includes(&self, event: &Event, now: u64) -> bool {
        if let Some(id) = self.selected {
            return id == event.id;
        }
        (self.all_outcomes || event.outcome.failed())
            && self.source.as_ref().is_none_or(|s| s == &event.source)
            && (!self.last_hour || now.saturating_sub(event.timestamp_ms) <= 3_600_000)
    }
}

pub(super) fn page(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
    let tr = |key| crate::ui::copy::tr_ref(s.cfg.general.language, key);
    let history = diagnostics::recent();
    let filter = &s.diagnostic_filter;
    let now = diagnostics::now();
    let mut rows = Vec::new();
    let gateway = crate::platform::gateway_bootstrap::status();
    let local_only = s.cfg.general.gateway_local_only
        || gateway == crate::platform::gateway_bootstrap::Status::Disabled;
    rows.push(super::row(
        k,
        "gateway-local-only",
        tr("Local-only mode"),
        tr("Use Desktop without automatically installing a local Gateway."),
        super::flag(
            k,
            "gateway-local-only",
            local_only,
            tr("Local-only mode"),
            |s| {
                let disabled = !(s.cfg.general.gateway_local_only
                    || crate::platform::gateway_bootstrap::status()
                        == crate::platform::gateway_bootstrap::Status::Disabled);
                s.edit(crate::config::ConfigEdit::GatewayLocalOnly(disabled));
                crate::platform::gateway_bootstrap::set_local_only(disabled);
            },
        ),
    ));
    rows.push(super::row(
        k,
        "gateway-setup",
        tr("Local Gateway"),
        tr(gateway.message()),
        rune::row()
            .gap(8.0)
            .child(
                super::button(tr(if super::super::gateway_setup::running(&gateway) {
                    "Show progress"
                } else if matches!(
                    gateway,
                    crate::platform::gateway_bootstrap::Status::NeedsConsent
                        | crate::platform::gateway_bootstrap::Status::Deferred
                ) {
                    "Install"
                } else {
                    "Retry Gateway setup"
                }))
                .disabled(
                    local_only
                        || (!gateway.retryable()
                            && !super::super::gateway_setup::running(&gateway))
                        || s.current_computer() != "local",
                )
                .on_press(|s: &mut UiState| {
                    s.show_gateway_setup(crate::platform::gateway_bootstrap::status())
                })
                .build(k),
            )
            .child(
                super::button(tr("Connect local Gateway"))
                    .disabled(
                        gateway != crate::platform::gateway_bootstrap::Status::Ready
                            || !s.can_connect_new_local_gateway(),
                    )
                    .on_press(|s: &mut UiState| s.switch_computer("local".into()))
                    .build(k),
            ),
    ));
    if filter.selected.is_some() {
        rows.push(super::row(
            k,
            "related",
            tr("Selected event"),
            "",
            super::button(tr("Show recent events"))
                .on_press(|s: &mut UiState| s.diagnostic_filter = Default::default())
                .build(k),
        ));
    } else {
        let mut sources = history
            .iter()
            .map(|event| event.source.clone())
            .collect::<Vec<_>>();
        sources.sort();
        sources.dedup();
        let mut names = vec![tr("All sources")];
        names.extend(sources.iter().map(String::as_str));
        let index = filter
            .source
            .as_ref()
            .and_then(|name| sources.iter().position(|s| s == name))
            .map_or(0, |i| i + 1);
        let choices = sources.clone();
        rows.push(super::row(
            k,
            "source",
            tr("Source"),
            "",
            super::select(
                s,
                k,
                layer,
                "Diagnostic source",
                &names,
                index,
                move |s, i| {
                    s.diagnostic_filter.source =
                        i.checked_sub(1).and_then(|i| choices.get(i).cloned())
                },
            ),
        ));
        rows.push(super::row(
            k,
            "status",
            tr("Outcome"),
            "",
            super::select(
                s,
                k,
                layer,
                "Diagnostic outcome",
                &[tr("Errors"), tr("All outcomes")],
                usize::from(filter.all_outcomes),
                |s, i| s.diagnostic_filter.all_outcomes = i == 1,
            ),
        ));
        rows.push(super::row(
            k,
            "time",
            tr("Time"),
            "",
            super::select(
                s,
                k,
                layer,
                "Diagnostic time",
                &[tr("All retained events"), tr("Last hour")],
                usize::from(filter.last_hour),
                |s, i| s.diagnostic_filter.last_hour = i == 1,
            ),
        ));
    }
    let mut body = vec![super::group(
        k,
        "diagnostic-filters",
        tr("Diagnostics"),
        rows,
    )];
    let mut events = Vec::new();
    let matched: Vec<_> = history.iter().filter(|e| filter.includes(e, now)).collect();
    let page = filter.page.min(matched.len().saturating_sub(1) / 20);
    for e in matched.iter().skip(page * 20).take(20) {
        let elapsed = now.saturating_sub(e.timestamp_ms) / 1000;
        let age = if elapsed < 60 {
            format!("{elapsed}s")
        } else if elapsed < 3600 {
            format!("{}m", elapsed / 60)
        } else {
            format!("{}h", elapsed / 3600)
        };
        let report = (*e).clone();
        let version = e
            .version
            .as_ref()
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        events.push(super::row(
            k,
            &e.id.to_string(),
            &format!("{} · {}", tr(e.action.label()), tr(e.outcome.label())),
            &format!("{}{version} · −{age} · {} ms", e.source, e.duration_ms),
            super::button(tr("Copy diagnostics"))
                .on_press_tx(move |_: &mut UiState, tx: &mut rune::Tx| {
                    tx.copy(serde_json::to_string_pretty(&report).unwrap_or_default())
                })
                .build(k),
        ));
    }
    if events.is_empty() {
        body.push(super::empty(
            k,
            "empty-diagnostics",
            tr("No matching events"),
            "",
        ));
    } else {
        body.push(super::group(k, "diagnostic-events", tr("Events"), events));
    }
    if matched.len() > 20 {
        body.push(
            rune::row()
                .gap(8.0)
                .child(
                    super::button(tr("Newer"))
                        .disabled(page == 0)
                        .on_press(move |s: &mut UiState| {
                            s.diagnostic_filter.page = page.saturating_sub(1)
                        })
                        .build(k),
                )
                .child(
                    super::button(tr("Older"))
                        .disabled((page + 1) * 20 >= matched.len())
                        .on_press(move |s: &mut UiState| s.diagnostic_filter.page = page + 1)
                        .build(k),
                ),
        );
    }
    body
}
