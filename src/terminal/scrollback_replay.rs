//! Replay recorded pane reads through the production read path and measure
//! what the chat view would have been served.
//!
//! The reads come from `MUQUN_SCROLLBACK_TRACE_DIR` (see `scrollback::trace_read`):
//! one JSON line per read, carrying the rows Herdr answered and whether the pane
//! was kept and owned its screen at the time. Each read goes through exactly
//! what `pane_parts` does with it -- `ScrollbackStore::serve_read` with the
//! recorded flags (set through `observe`, as the approval watcher sets them),
//! then `parts::blank_frozen_status` and `parts::normalize_json` with Claude's
//! dictionary -- and every answer is checked for two things the owner sees as
//! duplicates: a part repeated within one answer, and a status row or prompt
//! box left above the live bottom area.
//!
//! ```text
//! MUQUN_REPLAY_TRACE=<file-or-dir> cargo test replay_recorded -- --ignored --nocapture
//! ```

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::platform::parts;
use crate::terminal::scrollback::ScrollbackStore;

/// What the App asks `/parts` for when a chat view opens
/// (`INITIAL_PANE_OUTPUT_LINES`).
const APP_LINES: usize = 240;

/// How deep the live bottom area of a Claude pane reaches: spinner, prompt box,
/// mode line and a short roster.
const LIVE_ROWS: usize = 12;

pub(crate) struct Read {
    pub session: String,
    pub pane: String,
    pub source: String,
    pub format: String,
    pub kept: bool,
    pub owns_screen: bool,
    pub text: String,
    pub ts_ms: u64,
}

pub(crate) fn parse_trace(body: &str) -> Vec<Read> {
    body.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: Value = serde_json::from_str(line).expect("one JSON read per line");
            let field = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
            Read {
                session: field("session"),
                pane: field("pane"),
                source: field("source"),
                format: field("format"),
                kept: value["kept"].as_bool().unwrap_or(false),
                owns_screen: value["owns_screen"].as_bool().unwrap_or(false),
                text: value["rows"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .map(|row| row.as_str().unwrap_or_default())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default(),
                ts_ms: value["ts_ms"].as_u64().unwrap_or(0),
            }
        })
        .collect()
}

/// One problem found in one served answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Offence {
    /// A part whose text equals an earlier part of the same answer.
    Repeated(String),
    /// A status row or prompt box above the live bottom area.
    Frozen(String),
}

fn normalised(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parts that legitimately recur: background agents announced one by one, and
/// a waiting line whose count changes (equal texts there would still count).
fn recurs_legitimately(text: &str) -> bool {
    text.starts_with("⎿ Backgrounded agent")
}

fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 10 && trimmed.chars().all(|c| c == '─')
}

/// A spinner row with a running timer, `✻ Channeling… (7m 6s · …)`.
fn is_timed_spinner(line: &str) -> bool {
    let mut chars = line.chars();
    chars.next().is_some_and(|glyph| "✻✽✳✢∗·*✶".contains(glyph))
        && chars.next() == Some(' ')
        && line.contains("… (")
}

/// Every problem in one `/parts` answer, measured on the text it normalized and
/// on the parts it produced.
pub(crate) fn offences(chat_text: &str, parts: &[Value]) -> Vec<Offence> {
    let mut found = Vec::new();

    let mut seen = HashSet::new();
    for part in parts {
        let text = normalised(part["fallback_text"].as_str().unwrap_or_default());
        if text.is_empty() || recurs_legitimately(&text) {
            continue;
        }
        if !seen.insert(text.clone()) {
            found.push(Offence::Repeated(text));
        }
    }

    let lines: Vec<&str> = chat_text.split('\n').collect();
    let mut bottom = lines.len();
    while bottom > 0 && lines[bottom - 1].trim().is_empty() {
        bottom -= 1;
    }
    let live = bottom.saturating_sub(LIVE_ROWS);
    for (row, line) in lines.iter().enumerate().take(live) {
        let prompt_box = is_rule(line)
            && lines
                .get(row + 1)
                .is_some_and(|next| next.trim_start().starts_with('❯'));
        if prompt_box || is_timed_spinner(line) || line.contains("bypass permissions") {
            found.push(Offence::Frozen(normalised(line)));
        }
    }
    found
}

/// What `serve_read` answers for this read, with the pane's recorded flags
/// observed first, as the approval watcher would have.
fn serve(store: &mut ScrollbackStore, read: &Read, lines: usize) -> String {
    store.observe(
        &read.session,
        &json!({
            "pane_id": read.pane,
            "scroll": {
                "max_offset_from_bottom": if read.kept { 0 } else { 1000 },
            },
            "foreground_command": if read.owns_screen { "nvim" } else { "claude" },
        }),
    );
    store.serve_read(
        &read.session,
        &read.pane,
        &read.source,
        &read.format,
        &read.text,
        lines,
    )
}

/// Text with its SGR/CSI/OSC escapes removed: what a reader sees of an ANSI row.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Offences measured over one kind of answer.
#[derive(Default)]
pub(crate) struct Tally {
    pub answers: usize,
    /// Answers holding at least one repeated part / frozen area.
    pub repeating: usize,
    pub frozen: usize,
    /// Distinct offences, with the read they first appeared at.
    pub first_seen: Vec<(usize, Offence)>,
    known: HashSet<Offence>,
}

impl Tally {
    fn add(&mut self, index: usize, found: Vec<Offence>) {
        self.answers += 1;
        if found.iter().any(|o| matches!(o, Offence::Repeated(_))) {
            self.repeating += 1;
        }
        if found.iter().any(|o| matches!(o, Offence::Frozen(_))) {
            self.frozen += 1;
        }
        for offence in found {
            if self.known.insert(offence.clone()) {
                self.first_seen.push((index, offence));
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Report {
    pub reads: usize,
    pub passed_through: usize,
    pub flips: usize,
    /// `/parts?lines=240` after every text read -- what the chat view got.
    pub parts: Tally,
    /// The 240-row ANSI window after every ANSI read, escapes stripped, raw
    /// (no blanking) -- what the terminal view got.
    pub ansi_raw: Tally,
    /// The same window put through the chat derivation, as if `/parts` read it.
    pub ansi_parts: Tally,
    /// Reads after which the served ANSI window held a copied block, and the
    /// copied blocks in the deepest window after the last read.
    pub windows_with_blocks: usize,
    pub first_block_at: Option<usize>,
    pub final_blocks: Vec<String>,
}

fn derive(served: &str) -> (String, Vec<Value>) {
    let chat_text = parts::blank_frozen_status(served);
    let parts = parts::normalize_json(&chat_text, parts::dictionary_for(Some("claude")));
    (chat_text, parts)
}

/// Runs of [`BLOCK_ROWS`] or more consecutive text rows written down twice:
/// the buffer copying a screen into itself, as opposed to one line a transcript
/// genuinely repeats. Returned by the first row of each copy.
pub(crate) fn copied_blocks(text: &str) -> Vec<String> {
    let rows: Vec<String> = text
        .split('\n')
        .map(normalised)
        .filter(|row| !row.is_empty())
        .collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < rows.len() {
        let mut copied = 0;
        for later in at + 1..rows.len() {
            let run = (0..)
                .take_while(|step| {
                    at + step < later
                        && later + step < rows.len()
                        && rows[at + step] == rows[later + step]
                })
                .count();
            if run >= BLOCK_ROWS {
                copied = run;
                break;
            }
        }
        if copied > 0 {
            found.push(rows[at].clone());
            at += copied;
        } else {
            at += 1;
        }
    }
    found
}

/// How many consecutive text rows make a copied block rather than a repeat.
const BLOCK_ROWS: usize = 4;

pub(crate) fn replay(reads: &[Read]) -> Report {
    let mut store = ScrollbackStore::default();
    let mut report = Report::default();
    let mut last_flags: HashMap<String, (bool, bool)> = HashMap::new();
    for (index, read) in reads.iter().enumerate() {
        report.reads += 1;
        if !read.kept {
            report.passed_through += 1;
        }
        let flags = (read.kept, read.owns_screen);
        if let Some(previous) = last_flags.insert(read.pane.clone(), flags) {
            if previous != flags {
                report.flips += 1;
            }
        }
        let served = serve(&mut store, read, APP_LINES);
        if read.format == "text" {
            let (chat_text, parts) = derive(&served);
            report.parts.add(index, offences(&chat_text, &parts));
        } else {
            let plain = strip_ansi(&served);
            let raw_parts = parts::normalize_json(&plain, parts::dictionary_for(Some("claude")));
            report.ansi_raw.add(index, offences(&plain, &raw_parts));
            let (chat_text, parts) = derive(&plain);
            report.ansi_parts.add(index, offences(&chat_text, &parts));
            if !copied_blocks(&plain).is_empty() {
                report.windows_with_blocks += 1;
                report.first_block_at.get_or_insert(index);
            }
        }
    }
    let mut deep = ScrollbackStore::default();
    let mut last_deep = String::new();
    for read in reads.iter().filter(|read| read.format != "text") {
        let served = serve(&mut deep, read, 5_000);
        report.final_blocks = copied_blocks(&strip_ansi(&served));
        last_deep = served;
    }
    if let Some(dir) = std::env::var_os("MUQUN_REPLAY_DUMP") {
        if let Some(read) = reads.first() {
            let path =
                std::path::Path::new(&dir).join(format!("{}.txt", read.pane.replace(':', "_")));
            std::fs::write(path, strip_ansi(&last_deep)).expect("dump");
        }
    }
    report
}

fn trace_files(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    if path.is_dir() {
        let mut files: Vec<_> = std::fs::read_dir(path)
            .expect("trace dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        files
    } else {
        vec![path.to_owned()]
    }
}

fn print_tally(label: &str, tally: &Tally) {
    let repeated = tally
        .first_seen
        .iter()
        .filter(|(_, o)| matches!(o, Offence::Repeated(_)))
        .count();
    println!(
        "  {label}: {} answers, {} with repeated parts, {} with frozen areas; \
         distinct repeated {repeated}, distinct frozen {}",
        tally.answers,
        tally.repeating,
        tally.frozen,
        tally.first_seen.len() - repeated,
    );
    for (index, offence) in tally.first_seen.iter().take(10) {
        let text = match offence {
            Offence::Repeated(text) => format!("repeated: {text}"),
            Offence::Frozen(text) => format!("frozen:   {text}"),
        };
        println!(
            "    read {index:>5}  {}",
            text.chars().take(120).collect::<String>()
        );
    }
}

fn print_report(name: &str, report: &Report) {
    println!(
        "{name}: {} reads ({} passed through, {} flag flips)",
        report.reads, report.passed_through, report.flips,
    );
    print_tally("/parts (text reads)", &report.parts);
    print_tally("ansi window raw", &report.ansi_raw);
    print_tally("ansi window derived", &report.ansi_parts);
    println!(
        "  copied blocks (>= {BLOCK_ROWS} text rows): {} served windows, first at read {:?}; \
         {} in the whole buffer at the end",
        report.windows_with_blocks,
        report.first_block_at,
        report.final_blocks.len(),
    );
    for block in report.final_blocks.iter().take(5) {
        println!("    {}", block.chars().take(100).collect::<String>());
    }
}

/// Replay every recorded read named by `MUQUN_REPLAY_TRACE` (a `.jsonl` file or
/// a directory of them) and print what the chat view was served.
#[test]
#[ignore = "needs MUQUN_REPLAY_TRACE pointing at recorded reads"]
fn replay_recorded_reads() {
    let path = std::env::var("MUQUN_REPLAY_TRACE").expect("MUQUN_REPLAY_TRACE");
    for file in trace_files(std::path::Path::new(&path)) {
        let body = std::fs::read_to_string(&file).expect("trace file");
        let reads = parse_trace(&body);
        let report = replay(&reads);
        print_report(&file.display().to_string(), &report);
    }
}

/// Every read after which the held buffer grew by more than `MUQUN_REPLAY_JUMP`
/// rows (default 30): the reads that went on whole.
#[test]
#[ignore = "needs MUQUN_REPLAY_TRACE pointing at one recorded pane"]
fn replay_recorded_jumps() {
    let path = std::env::var("MUQUN_REPLAY_TRACE").expect("MUQUN_REPLAY_TRACE");
    let jump: usize = std::env::var("MUQUN_REPLAY_JUMP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let reads = parse_trace(&std::fs::read_to_string(path).expect("trace"));
    let mut store = ScrollbackStore::default();
    let mut depth: HashMap<String, usize> = HashMap::new();
    for (index, read) in reads.iter().enumerate() {
        serve(&mut store, read, APP_LINES);
        let key = format!("{}/{}", read.source, read.format);
        let now = store.held_rows(&read.session, &read.pane, &read.source, &read.format);
        let before = depth.insert(key.clone(), now).unwrap_or(0);
        if now > before + jump {
            println!(
                "read {index} ts {} {key}: {before} -> {now} (+{})",
                read.ts_ms,
                now - before
            );
        }
    }
}

/// Footer rows left above the live bottom area: rows, more than [`LIVE_ROWS`]
/// from the end, shaped like one of the last text rows held (a mode line, a
/// token counter, an input box border) or opening a prompt box. Shapes ignore
/// digits, so a footer frozen with an older count is still found.
pub(crate) fn frozen_footers(text: &str) -> Vec<String> {
    let shape = |row: &str| -> String {
        let mut out = String::new();
        for c in normalised(row).chars() {
            if !c.is_ascii_digit() {
                out.push(c);
            } else if !out.ends_with('#') {
                out.push('#');
            }
        }
        out
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let mut bottom = lines.len();
    while bottom > 0 && lines[bottom - 1].trim().is_empty() {
        bottom -= 1;
    }
    let live = bottom.saturating_sub(LIVE_ROWS);
    let footer: HashSet<String> = lines[..bottom]
        .iter()
        .rev()
        .filter(|row| !row.trim().is_empty() && !is_rule(row))
        .take(3)
        .map(|row| shape(row))
        .collect();
    (0..live)
        .filter(|row| {
            let line = lines[*row];
            let prompt_box = is_rule(line)
                && lines
                    .get(row + 1)
                    .is_some_and(|next| next.trim_start().starts_with('❯'));
            prompt_box || (!line.trim().is_empty() && footer.contains(&shape(line)))
        })
        .map(|row| normalised(lines[row]))
        .collect()
}

/// Fold `reads` into a fresh store and answer the deepest window of the last
/// one's shape, escapes stripped.
pub(crate) fn fold_reads(reads: &[Read]) -> String {
    let mut store = ScrollbackStore::default();
    let mut served = String::new();
    for read in reads {
        served = serve(&mut store, read, 5_000);
    }
    strip_ansi(&served)
}

/// Replay reads `MUQUN_REPLAY_RANGE` (`first-last`, line numbers from 0) of
/// one trace into a fresh store and print what the deepest window holds.
#[test]
#[ignore = "needs MUQUN_REPLAY_TRACE and MUQUN_REPLAY_RANGE"]
fn replay_recorded_window() {
    let path = std::env::var("MUQUN_REPLAY_TRACE").expect("MUQUN_REPLAY_TRACE");
    let range = std::env::var("MUQUN_REPLAY_RANGE").expect("MUQUN_REPLAY_RANGE");
    let (first, last) = range.split_once('-').expect("first-last");
    let (first, last): (usize, usize) = (first.parse().unwrap(), last.parse().unwrap());
    let reads = parse_trace(&std::fs::read_to_string(path).expect("trace"));
    let format = std::env::var("MUQUN_REPLAY_FORMAT").unwrap_or_else(|_| "ansi".into());
    let window: Vec<Read> = reads
        .into_iter()
        .enumerate()
        .filter(|(index, read)| (first..=last).contains(index) && read.format == format)
        .map(|(_, read)| read)
        .collect();
    let held = fold_reads(&window);
    if let Some(out) = std::env::var_os("MUQUN_REPLAY_DUMP") {
        std::fs::write(out, &held).expect("dump");
    }
    println!(
        "window {range}: {} reads -> {} rows; copied blocks {:?}; frozen footers {:?}",
        window.len(),
        held.split('\n').count(),
        copied_blocks(&held),
        frozen_footers(&held)
    );
}

/// A checked-in fixture: reads cut from a recorded pane, anonymised (letters
/// and ideographs through a fixed substitution, so equal rows stay equal), as
/// every distinct row once plus each read as indices into them.
fn fixture(body: &str) -> Vec<Read> {
    let value: Value = serde_json::from_str(body).expect("fixture json");
    let format = value["format"].as_str().expect("format").to_owned();
    let rows: Vec<&str> = value["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row.as_str().expect("row"))
        .collect();
    value["reads"]
        .as_array()
        .expect("reads")
        .iter()
        .map(|read| Read {
            session: "s".into(),
            pane: "p".into(),
            source: "recent_unwrapped".into(),
            format: format.clone(),
            kept: true,
            owns_screen: false,
            text: read
                .as_array()
                .expect("read")
                .iter()
                .map(|id| rows[id.as_u64().expect("row id") as usize])
                .collect::<Vec<_>>()
                .join("\n"),
            ts_ms: 0,
        })
        .collect()
}

macro_rules! fixture {
    ($name:literal) => {
        fixture(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/scrollback/",
            $name,
            ".json"
        )))
    };
}

/// Every fixture folds to a buffer with no screen copied into it and no footer
/// frozen above the live one. Each of them did both before rows were matched
/// on their text (see the module doc of `scrollback`, "Placing a read").
#[test]
fn recorded_panes_fold_without_copies_or_frozen_footers() {
    for (name, reads) in [
        // A repaint that re-encoded every row's colours and moved the
        // transcript down eight rows when a feedback box went away.
        ("claude-shifted-repaint", fixture!("claude-shifted-repaint")),
        // Reads taken while Claude Code was halfway through a repaint.
        ("claude-mid-redraw", fixture!("claude-mid-redraw")),
        // The first read after a 57-second gap in the reads.
        ("claude-after-gap", fixture!("claude-after-gap")),
        // The pane resized to 75 columns and its screen re-wrapped.
        ("claude-resize", fixture!("claude-resize")),
        // `/usage` drawn over the whole screen, then closed.
        ("claude-usage-panel", fixture!("claude-usage-panel")),
        // Codex pins the current prompt to the top row while output scrolls.
        ("codex-pinned-prompt", fixture!("codex-pinned-prompt")),
        // OpenCode read again after 55 minutes: a new screen, a new footer.
        ("opencode-after-gap", fixture!("opencode-after-gap")),
        // OpenCode scrolling its transcript beside a sidebar that stays put.
        ("opencode-sidebar", fixture!("opencode-sidebar")),
    ] {
        let held = fold_reads(&reads);
        let copies: Vec<String> = copied_blocks(&held)
            .into_iter()
            // `/status` run twice, minutes apart: a real repeat (see below).
            .filter(|block| !(name == "codex-pinned-prompt" && block == "/zahabz"))
            .collect();
        assert!(copies.is_empty(), "{name}: copied {copies:?}");
        let frozen = frozen_footers(&held);
        assert!(frozen.is_empty(), "{name}: frozen {frozen:?}");
    }
}

/// Codex: the prompt pinned to the top row is held once, and the two `/status`
/// panels the pane really printed are both held.
#[test]
fn a_pinned_codex_prompt_is_held_once_and_real_repeats_stay() {
    let reads = fixture!("codex-pinned-prompt");
    let pinned = normalised(&strip_ansi(
        reads[0].text.split('\n').next().expect("top row"),
    ));
    let held = fold_reads(&reads);
    let rows: Vec<String> = held.split('\n').map(normalised).collect();
    assert_eq!(rows.iter().filter(|row| **row == pinned).count(), 1);
    assert_eq!(rows.iter().filter(|row| *row == "/zahabz").count(), 2);
}

/// OpenCode: a transcript row that scrolled beside the sidebar, and so carries
/// a different piece of it on each read, is still held once.
#[test]
fn a_row_scrolled_beside_a_sidebar_is_held_once() {
    let held = fold_reads(&fixture!("opencode-sidebar"));
    // "Clarifying token use", through the fixture's substitution.
    assert_eq!(held.matches("Jshypmfpun avrlu bzl").count(), 1);
}
