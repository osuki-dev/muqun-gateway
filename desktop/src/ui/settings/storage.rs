//! Settings → Storage: what the desktop keeps on disk, as one bar split by
//! kind, and a button for the part it can rebuild.

use rune::{col, el, row as hrow, text, Color, El};
use rune_kit::{ButtonVariant, Kit};

use super::{group, row};
use crate::model::commands::Command;
use crate::model::storage::human;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

/// A kind's swatch: a dot in the legend, a run of the bar.
fn dot(color: Color) -> El {
    el().size(8.0, 8.0).radius(4.0).fill(color).a11y_hidden()
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let look = s.look();
    let report = s.storage.clone().unwrap_or_default();
    let scanned = s.storage.is_some();
    let size = |n: u64| if scanned { human(n) } else { "…".to_string() };
    let accent = k.c.bg_accent;
    let kinds: [(&str, &str, &str, u64, Color); 3] = [
        (
            "cache",
            tr_ref(language, "Cache", "缓存"),
            tr_ref(
                language,
                "Theme previews and the gallery list. Rebuilt when needed.",
                "主题预览与图库列表，需要时会重新生成。",
            ),
            report.cache(),
            accent,
        ),
        (
            "uploads",
            tr_ref(language, "Attachments", "附件副本"),
            tr_ref(
                language,
                "Copies of files and pictures you attached to prompts.",
                "你在提示中附加的文件与图片的副本。",
            ),
            report.uploads,
            accent.alpha(0.55),
        ),
        (
            "other",
            tr_ref(language, "Sessions and settings", "会话与设置"),
            tr_ref(
                language,
                "Window state, drafts and this configuration.",
                "窗口状态、草稿与这些设置。",
            ),
            report.other,
            k.c.text_faint,
        ),
    ];
    let total = report.total();

    // The total, large, over a bar each kind takes its share of.
    let mut bar = hrow()
        .key("storage-bar")
        .w_full()
        .h(10.0)
        .gap(2.0)
        .radius(5.0)
        .clip_rounded(5.0)
        .fill(k.c.text_base.alpha(0.06));
    if scanned && total > 0 {
        for (key, _, _, n, color) in &kinds {
            if *n == 0 {
                continue;
            }
            // A sliver still shows: every kind on disk gets a visible run.
            let share = (*n as f32 / total as f32).max(0.015);
            bar = bar.child(
                el().key(format!("bar-{key}"))
                    .h_full()
                    .grow(share)
                    .fill(*color),
            );
        }
    }
    let state_dir = report
        .state_dir
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let summary = col()
        .key("storage-summary")
        .w_full()
        .gap(12.0)
        .child(
            hrow()
                .key("storage-total")
                .gap(10.0)
                .items_end()
                .child(
                    text(size(total))
                        .key("storage-total-size")
                        .font(rune::TextStyle {
                            size: 28.0,
                            line_height: 34.0,
                            ..k.ty.ui_medium
                        })
                        .color(k.c.text_base),
                )
                .child(
                    text(tr_ref(language, "on this computer", "存放在这台电脑上"))
                        .font(k.ty.ui)
                        .color(k.c.text_muted)
                        .pb(5.0),
                ),
        )
        .child(look.glow(bar, accent, 0.18))
        .children(
            (!state_dir.is_empty())
                .then(|| super::long_text(k, "storage-dir", state_dir, k.ty.small)),
        );

    let legend = kinds
        .iter()
        .map(|(key, title, detail, n, color)| {
            row(
                k,
                &format!("storage-{key}"),
                title,
                detail,
                hrow()
                    .key("size")
                    .gap(8.0)
                    .center_y()
                    .child(dot(*color))
                    .child(
                        // Sizes line up on their last digit, at the column's edge.
                        hrow()
                            .key("n")
                            .min_w(64.0)
                            .justify_end()
                            .child(text(size(*n)).font(k.ty.ui_medium).color(k.c.text_base)),
                    ),
            )
        })
        .collect();

    vec![
        summary,
        group(
            k,
            "storage-kinds",
            tr_ref(language, "What takes the space", "占用明细"),
            legend,
        ),
        group(
            k,
            "storage-clean",
            tr_ref(language, "Clean up", "清理"),
            vec![row(
                k,
                "storage-clear",
                tr_ref(language, "Clear cache", "清除缓存"),
                tr_ref(
                    language,
                    "Frees the cache now. Sessions, attachments and settings stay.",
                    "立即释放缓存。会话、附件与设置不受影响。",
                ),
                super::button(tr_ref(language, "Clear cache", "清除缓存"))
                    .variant(ButtonVariant::Danger)
                    .disabled(!scanned || report.cache() == 0)
                    .on_press(|s: &mut UiState| {
                        s.storage = None;
                        s.send(Command::ClearStorage);
                    })
                    .build(k)
                    .key("storage-clear"),
            )],
        ),
    ]
}
