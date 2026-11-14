//! A change drawn as rows inside a card (App `InlineDiffRows`): line
//! numbers, a tinted line per addition and removal, word changes marked,
//! syntax colours, a folded gap between hunks. Plain rows rather than a diff
//! view with its own scroll, so the wheel over a diff scrolls the timeline.

use rune::{col, icon, rich, row, text, Color, Cursor, El, Span};
use rune_kit::diff::{FileDiff, Hunk, LineKind};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use super::{flip, glyphs, Tcx};
use crate::model::commands::Command;
use muqun_gateway::agents::AgentSessionId;

/// Lines a card shows before "Show all".
const LINES_SHOWN: usize = 80;

/// Colour and word-change background per byte segment of one line.
fn line_spans(
    theme: &SyntaxTheme,
    lang: Option<&str>,
    text: &str,
    changes: &[std::ops::Range<usize>],
    change_bg: Color,
) -> Vec<Span> {
    let highlights = syntax::highlight(lang, text);
    if changes.is_empty() {
        return theme.spans(&highlights, 0);
    }
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for (r, _) in highlights.iter() {
        cuts.extend([r.start, r.end]);
    }
    for r in changes {
        cuts.extend([r.start, r.end]);
    }
    cuts.retain(|c| *c <= text.len() && text.is_char_boundary(*c));
    cuts.sort_unstable();
    cuts.dedup();
    let mut spans = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a == b {
            continue;
        }
        let color = highlights
            .iter()
            .find(|(r, _)| r.start <= a && b <= r.end)
            .map(|(_, p)| theme.paint(*p));
        let changed = changes.iter().any(|r| r.start <= a && b <= r.end);
        if color.is_none() && !changed {
            continue;
        }
        let mut sp = Span::new(a, b);
        sp.color = color;
        if changed {
            sp.bg = Some(change_bg);
        }
        spans.push(sp);
    }
    spans
}

fn number(k: &Kit, n: Option<u32>, color: Color) -> El {
    row().w(30.0).justify_end().child(
        text(n.map(|n| n.to_string()).unwrap_or_default())
            .font(k.ty.mono)
            .color(color)
            .single_line(),
    )
}

fn file_header(cx: &Tcx, fi: usize, file: &FileDiff) -> El {
    let k = cx.k;
    let path = file.path().to_string();
    let asid = cx.asid.to_string();
    let (dir, name) = match path.rfind('/') {
        Some(idx) => (&path[..=idx], &path[idx + 1..]),
        None => ("", path.as_str()),
    };

    let mut left = row().gap(6.0).center_y().shrink().grow(1.0).child(
        icon(&glyphs::EDIT_FILE, 13.0)
            .color(k.c.icon_faint)
            .a11y_hidden(),
    );

    if !dir.is_empty() {
        left = left.child(
            text(dir.to_string())
                .font(k.ty.small)
                .color(k.c.text_faint)
                .single_line()
                .truncate_head(),
        );
    }
    left = left.child(
        text(name.to_string())
            .font(k.ty.small_medium)
            .color(k.c.text_base)
            .single_line()
            .truncate(),
    );

    let mut stats = row().gap(6.0).center_y();
    if file.additions() > 0 {
        stats = stats.child(
            text(format!("+{}", file.additions()))
                .font(k.ty.small_medium)
                .color(k.c.success.fg),
        );
    }
    if file.deletions() > 0 {
        stats = stats.child(
            text(format!("−{}", file.deletions()))
                .font(k.ty.small_medium)
                .color(k.c.danger.fg),
        );
    }

    let p = path.clone();
    let asid_c = asid.clone();
    let diff_btn = Button::new(cx.t("Diff"))
        .variant(ButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .on_press_tx(move |s: &mut crate::ui::UiState, tx: &mut rune::Tx| {
            s.send(Command::OpenDiff {
                asid: AgentSessionId(asid_c.clone()),
                file: Some(p.clone()),
            });
            s.show_diff = true;
            if let Some(diff) = &mut s.diff {
                if let Some(idx) = diff.file_index(&p) {
                    s.diff_active_file = idx;
                    diff.scroll_to_file(idx, tx);
                }
            }
        })
        .build(k)
        .key(format!("diff-btn-{fi}"));

    let copy_path = path.clone();
    let copy_btn = k
        .interactive(
            row().size(20.0, 20.0).center().radius(k.radius.sm).child(
                icon(&rune_kit::glyphs::COPY, 12.0)
                    .color(k.c.icon_faint)
                    .a11y_hidden(),
            ),
        )
        .cursor(Cursor::Pointer)
        .role("button")
        .label(cx.t("Copy path").to_string())
        .on_press_tx(move |_: &mut crate::ui::UiState, tx: &mut rune::Tx| {
            tx.copy(copy_path.clone())
        })
        .key(format!("copy-path-{fi}"));

    let bar = row()
        .key(format!("f-bar-{fi}"))
        .w_full()
        .px(10.0)
        .py(5.0)
        .gap(8.0)
        .center_y()
        .fill(k.c.layer_01)
        .child(left)
        .child(stats)
        .child(copy_btn)
        .child(diff_btn);

    let mut col_wrap = col().key(format!("f{fi}")).w_full();
    if fi > 0 {
        col_wrap = col_wrap.child(col().h(1.0).w_full().fill(k.c.border_weak));
    }
    col_wrap
        .child(bar)
        .child(col().h(1.0).w_full().fill(k.c.border_weak))
}

fn hunk_separator(k: &Kit, fi: usize, hi: usize, hunk: &Hunk) -> El {
    let header_text = hunk.header();
    let (hunk_range, section) = match header_text.split_once("@@ ") {
        Some((prefix, rest)) => (format!("{prefix}@@"), rest.trim().to_string()),
        None => (header_text, String::new()),
    };

    let mut bar = row()
        .key(format!("gap-bar-{fi}-{hi}"))
        .w_full()
        .px(10.0)
        .py(3.0)
        .gap(8.0)
        .center_y()
        .fill(k.alpha(k.c.layer_02, 0.6))
        .child(
            text(hunk_range)
                .font(k.ty.mono)
                .color(k.c.text_accent)
                .single_line(),
        );

    if !section.is_empty() {
        bar = bar.child(
            text(section)
                .font(k.ty.mono)
                .color(k.c.text_faint)
                .single_line()
                .truncate()
                .shrink()
                .grow(1.0),
        );
    }

    col()
        .key(format!("gap{fi}-{hi}"))
        .w_full()
        .child(col().h(1.0).w_full().fill(k.alpha(k.c.border_weak, 0.6)))
        .child(bar)
        .child(col().h(1.0).w_full().fill(k.alpha(k.c.border_weak, 0.6)))
}

/// The rows of `files`, at most [`LINES_SHOWN`] lines unless opened all.
pub fn rows(cx: &Tcx, id: &str, files: &[FileDiff]) -> El {
    let k = cx.k;
    let theme = SyntaxTheme::from_kit(k);
    let all_key = format!("{id}/all");
    let show_all = cx.is_open(&all_key, false);
    let budget = if show_all { usize::MAX } else { LINES_SHOWN };
    let total: usize = files
        .iter()
        .map(|f| f.hunks.iter().map(|h| h.lines.len()).sum::<usize>())
        .sum();
    let (add_bg, del_bg) = (
        k.alpha(k.c.success.icon, 0.10),
        k.alpha(k.c.danger.icon, 0.10),
    );
    let (add_word, del_word) = (
        k.alpha(k.c.success.icon, 0.28),
        k.alpha(k.c.danger.icon, 0.28),
    );
    let mut shown = 0usize;
    let mut block = col()
        .key(format!("rows-{id}"))
        .w_full()
        .radius(k.radius.sm)
        .clip_rounded(k.radius.sm)
        .border(1.0, k.c.border_weak)
        .fill(k.c.surface_raised_base);

    'files: for (fi, file) in files.iter().enumerate() {
        let lang = file.language();
        block = block.child(file_header(cx, fi, file));

        for (hi, hunk) in file.hunks.iter().enumerate() {
            if hi > 0 || hunk.old_start > 1 || hunk.new_start > 1 || !hunk.section.is_empty() {
                block = block.child(hunk_separator(k, fi, hi, hunk));
            }
            for (li, line) in hunk.lines.iter().enumerate() {
                if shown >= budget {
                    break 'files;
                }
                shown += 1;
                let (bg, bar, word, mark, num_color) = match line.kind {
                    LineKind::Added => (
                        Some(add_bg),
                        Some(k.c.success.icon),
                        add_word,
                        "+",
                        k.alpha(k.c.success.fg, 0.85),
                    ),
                    LineKind::Removed => (
                        Some(del_bg),
                        Some(k.c.danger.icon),
                        del_word,
                        "−",
                        k.alpha(k.c.danger.fg, 0.85),
                    ),
                    LineKind::Context => (None, None, add_word, " ", k.c.text_faint),
                };
                let spans = line_spans(&theme, lang, &line.text, &line.changes, word);
                let mut r = row()
                    .key(format!("l{fi}-{hi}-{li}"))
                    .w_full()
                    .gap(6.0)
                    .pr(10.0)
                    .min_h(20.0)
                    .center_y()
                    .child(
                        col()
                            .w(3.0)
                            .self_stretch()
                            .fill(bar.unwrap_or(k.alpha(k.c.layer_01, 0.0))),
                    )
                    .child(number(k, line.old, num_color))
                    .child(number(k, line.new, num_color))
                    .child(
                        row().w(12.0).justify_center().child(
                            text(mark)
                                .font(k.ty.mono)
                                .color(bar.unwrap_or(k.c.text_faint)),
                        ),
                    )
                    .child(
                        rich(line.text.clone(), spans)
                            .font(k.ty.mono)
                            .color(k.c.text_base)
                            .wrap_anywhere()
                            .shrink()
                            .grow(1.0),
                    );
                if let Some(bg) = bg {
                    r = r.fill(bg);
                }
                block = block.child(r);
            }
        }
    }
    let mut wrap = col()
        .key(format!("diff-{id}"))
        .w_full()
        .gap(6.0)
        .child(block);
    if total > LINES_SHOWN {
        let label = if show_all {
            cx.t("Show less").to_string()
        } else if cx.zh() {
            format!(
                "{}: {} / {total}",
                cx.t("Expand remaining lines:"),
                total.saturating_sub(shown)
            )
        } else {
            format!(
                "Show remaining {} lines (total {total})",
                total.saturating_sub(shown)
            )
        };
        wrap = wrap.child(
            row().w_full().justify_center().py(2.0).child(
                Button::new(label)
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(flip(cx.asid, &all_key))
                    .build(k)
                    .key("all"),
            ),
        );
    }
    wrap
}
