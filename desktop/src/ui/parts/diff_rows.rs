//! A change drawn as rows inside a card (App `InlineDiffRows`): line
//! numbers, a tinted line per addition and removal, word changes marked,
//! syntax colours, a folded gap between hunks. Plain rows rather than a diff
//! view with its own scroll, so the wheel over a diff scrolls the timeline.

use rune::{col, rich, row, text, Color, El, Span};
use rune_kit::diff::{FileDiff, LineKind};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use super::{flip, Tcx};

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

fn number(k: &Kit, n: Option<u32>) -> El {
    text(n.map(|n| n.to_string()).unwrap_or_default())
        .font(k.ty.mono)
        .color(k.c.text_faint)
        .w(30.0)
        .single_line()
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
        .fill(k.c.surface_raised_base)
        .py(4.0);
    'files: for (fi, file) in files.iter().enumerate() {
        let lang = file.language();
        if files.len() > 1 {
            block = block.child(
                row()
                    .key(format!("f{fi}"))
                    .w_full()
                    .gap(8.0)
                    .px(10.0)
                    .pt(if fi == 0 { 2.0 } else { 8.0 })
                    .pb(4.0)
                    .center_y()
                    .child(
                        text(file.path().to_string())
                            .font(k.ty.small_medium)
                            .color(k.c.text_base)
                            .single_line()
                            .truncate_head()
                            .shrink()
                            .grow(1.0),
                    )
                    .child(
                        text(format!("+{}", file.additions()))
                            .font(k.ty.small)
                            .color(k.c.success.fg),
                    )
                    .child(
                        text(format!("−{}", file.deletions()))
                            .font(k.ty.small)
                            .color(k.c.danger.fg),
                    ),
            );
        }
        for (hi, hunk) in file.hunks.iter().enumerate() {
            if hi > 0 {
                block = block.child(
                    text(if hunk.section.is_empty() {
                        "⋯".to_string()
                    } else {
                        format!("⋯  {}", hunk.section)
                    })
                    .key(format!("gap{fi}-{hi}"))
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .single_line()
                    .truncate()
                    .px(10.0)
                    .py(2.0)
                    .w_full(),
                );
            }
            for (li, line) in hunk.lines.iter().enumerate() {
                if shown >= budget {
                    break 'files;
                }
                shown += 1;
                let (bg, bar, word, mark) = match line.kind {
                    LineKind::Added => (Some(add_bg), Some(k.c.success.icon), add_word, "+"),
                    LineKind::Removed => (Some(del_bg), Some(k.c.danger.icon), del_word, "−"),
                    LineKind::Context => (None, None, add_word, " "),
                };
                let spans = line_spans(&theme, lang, &line.text, &line.changes, word);
                let mut r = row()
                    .key(format!("l{fi}-{hi}-{li}"))
                    .w_full()
                    .gap(6.0)
                    .pr(10.0)
                    .child(
                        col()
                            .w(2.0)
                            .self_stretch()
                            .fill(bar.unwrap_or(k.alpha(k.c.layer_01, 0.0))),
                    )
                    .child(number(k, line.old))
                    .child(number(k, line.new))
                    .child(
                        text(mark)
                            .font(k.ty.mono)
                            .color(bar.unwrap_or(k.c.text_faint))
                            .w(10.0),
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
            cx.t("Show less", "收起").to_string()
        } else if cx.zh() {
            crate::ui::copy::localize(cx.lang, format!("显示全部 {total} 行"))
        } else {
            format!("Show all {total} lines")
        };
        wrap = wrap.child(
            Button::new(label)
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(flip(cx.asid, &all_key))
                .build(k)
                .key("all"),
        );
    }
    wrap
}
