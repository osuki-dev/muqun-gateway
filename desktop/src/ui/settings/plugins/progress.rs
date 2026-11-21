use crate::plugin::packages::{Operation, PackageStage};
use crate::ui::{
    copy::tr_ref,
    package_activity::{Job, Status},
    UiState,
};
use rune::{col, el, row, text, El};
use rune_kit::{Kit, Progress};

pub(super) fn package_progress(s: &UiState, k: &Kit, id: &str) -> Option<El> {
    s.plugin_packages.plugin(id).map(|job| view(s, k, job, id))
}

pub(super) fn install_progress(s: &UiState, k: &Kit) -> Option<El> {
    s.plugin_packages.install().filter(|job| {
        matches!(job.status, Status::Running(_)) || matches!(&job.operation, Operation::Install(source) if source.trim() == s.plugin_source.text.trim())
    }).map(|job| view(s, k, job, "install-form"))
}

fn view(s: &UiState, k: &Kit, job: &Job, id: &str) -> El {
    let lang = s.cfg.general.language;
    let mut body = col().w_full().min_w(0.0).gap(5.0);
    match &job.status {
        Status::Running(stage) => {
            let (label, progress) = match stage {
                PackageStage::Preparing => ("Preparing package…", Some(0.0)),
                PackageStage::Resolving => ("Resolving package with Bun…", None),
                PackageStage::Validating => ("Validating plugin…", Some(0.5)),
                PackageStage::Activating => ("Activating plugin…", Some(0.75)),
                PackageStage::Removing => ("Removing plugin…", None),
            };
            let label = tr_ref(lang, label);
            if progress.is_none()
                && (s.cfg.appearance.reduced_motion || s.system_reduced_motion.get())
            {
                body = body
                    .child(text(label).color(k.c.text_muted).font(k.ty.small))
                    .child(
                        row()
                            .role("progressbar")
                            .label(label)
                            .w_full()
                            .h(6.0)
                            .radius(k.radius.sm)
                            .fill(k.c.layer_02)
                            .child(
                                el().w_frac(0.25)
                                    .h_full()
                                    .fill(k.c.border_active)
                                    .radius(k.radius.sm),
                            ),
                    );
            } else {
                body = body.child(Progress::new(progress).label(label).build(k));
            }
        }
        Status::Completed => {
            let label = match job.operation {
                Operation::Install(_) => "Plugin installed",
                Operation::Update(_) => "Plugin updated",
                Operation::Remove(_) => "Plugin removed",
            };
            body = body.child(Progress::new(Some(1.0)).label(tr_ref(lang, label)).build(k));
        }
        Status::Failed(error) => {
            let op = job.operation.clone();
            body = body
                .child(
                    text(error)
                        .font(k.ty.small)
                        .color(k.c.danger.fg)
                        .min_w(0.0)
                        .wrap()
                        .max_lines(3)
                        .help_when_truncated(error),
                )
                .child(
                    row().child(
                        super::super::button(tr_ref(lang, "Retry"))
                            .disabled(s.plugin_packages.busy())
                            .on_press(move |s: &mut UiState| s.change_plugin_package(op.clone()))
                            .build(k)
                            .key(format!("plugin-retry-{id}")),
                    ),
                );
        }
    }
    body
}
