//! Reading an agent's pending permission menu out of a pane, and turning an
//! answer to it back into keystrokes.
//!
//! Agents ask for permission by drawing a menu and blocking: a question, a
//! numbered list of answers, one of them marked as the cursor's, and a footer of
//! key hints. Claude Code draws
//!
//! ```text
//!  Do you want to proceed?
//!  ❯ 1. Yes
//!    2. Yes, and don't ask again for: npm install *
//!    3. No
//!
//!  Esc to cancel · Tab to amend · ctrl+e to explain
//! ```
//!
//! and so, in their own words, do the others. That shape -- not any one agent's
//! wording -- is what this module recognizes, which is why it lives beside the
//! marker dictionaries rather than inside them: a menu is a pane *state*, not a
//! part of the transcript.
//!
//! Three rules keep the detector honest, because a false positive here would
//! have the phone offer to answer a question nobody asked:
//!
//! - **Only the tail counts.** A menu that has been answered is redrawn away by
//!   the agent, so a menu that is still pending is the last thing in the pane.
//!   Anything after the options other than blank lines and key hints means the
//!   pane has moved on.
//! - **The list must be a list.** Options are contiguous and numbered `1..n`
//!   with `n >= 2`. A stray `3. ` in somebody's output is not a menu.
//! - **It must look like a question.** Either the line above ends up carrying a
//!   `?`, or the answers themselves are yes/no shaped -- and the menu must show
//!   either a cursor or a key-hint footer, which every real one does.
//!
//! What the wire carries is deliberately small: the question, the answers, the
//! surrounding lines the agent drew, and a fingerprint. It never carries an
//! agent-specific concept, and the push notification built from it (see
//! [`push_options`]) carries no terminal content at all.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::i18n::{self, Locale};

/// How far back from the end of the pane a menu may start. A permission menu is
/// a screenful at most; looking further only invites matching old output.
const TAIL_LINES: usize = 80;
/// Key-hint lines the agent may draw under the options. Two is what agents use;
/// the slack is for wrapped or decorated footers.
const MAX_TRAILING_HINTS: usize = 4;
/// How far above the options to look for the question.
const QUESTION_LOOKBACK: usize = 12;
/// The agent's own framing of the request -- tool name, command, diff. Capped
/// because a diff preview can be arbitrarily tall.
const MAX_CONTEXT_LINES: usize = 14;
const MAX_CONTEXT_CHARS: usize = 800;
/// A menu with more answers than this is not a permission prompt.
const MAX_OPTIONS: usize = 12;
/// How far above the menu to look for the `Tool(input)` line that occasioned it.
const TOOL_LOOKBACK: usize = 30;
/// Digit keys select an option directly. Past this, the answer is driven with
/// arrow keys instead.
const MAX_DIGIT_OPTION: u32 = 9;

/// Glyphs an agent draws to mark the highlighted answer.
const CURSOR_GLYPHS: &[char] = &['❯', '›', '»', '▶', '▸', '→', '>', '*'];

/// Rules that open a menu region. Crossing one going up ends the context.
const HARD_RULE_CHARS: &[char] = &['─', '═', '━', '—'];
/// Rules drawn *inside* a menu, around a diff preview. Skipped, not treated as
/// a boundary, or a file preview would hide the "Create file" heading above it.
const SOFT_RULE_CHARS: &[char] = &['╌', '┄', '┈', '╍', '┅'];

/// Line-start glyphs that mean the transcript has resumed: a block, a result, a
/// prompt. The menu's context never crosses one.
const TRANSCRIPT_GLYPHS: &[char] = &['⏺', '⎿', '❯', '✻', '✽', '✳', '✢', '∗', '▪', '●', '└', '│'];

/// Substrings that make a line a key hint rather than content.
const HINT_MARKERS: &[&str] = &[
    "to cancel",
    "to confirm",
    "to select",
    "to amend",
    "to explain",
    "to expand",
    "to accept",
    "to reject",
    "to continue",
    "to exit",
    "esc)",
    "press ",
    "ctrl+",
    "shift+",
];

/// What answering an option would mean, read off the answer's own wording.
///
/// This is the only interpretation the gateway puts on a menu, and it exists so
/// a client can render an approve/deny affordance -- and so a push notification
/// can name the choices without quoting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allow this one time.
    Allow,
    /// Allow, and stop asking (for this session, this command, this host).
    AllowAlways,
    /// Refuse.
    Deny,
    /// Something else the agent offered; only its index is meaningful.
    Other,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::AllowAlways => "allow_always",
            Decision::Deny => "deny",
            Decision::Other => "other",
        }
    }

    /// A label safe to put in a push notification: derived from the decision,
    /// never from the agent's text, so nothing from the terminal leaves the
    /// host in a notification payload. The native adapters label an `approval`
    /// part's answers with it for the same reason -- a protocol's own wording
    /// for "always" embeds the pattern it would save.
    ///
    /// Because the gateway wrote these four strings, the gateway may translate
    /// them, and that is precisely what [`Decision::as_str`] above may never
    /// do: the label is prose a person reads, the `as_str` value is vocabulary
    /// a client dispatches on. They are separate functions for that reason and
    /// only that reason.
    pub fn public_label(self, locale: Locale, index: u32) -> String {
        match self {
            Decision::Allow => i18n::t(locale, "Approve").to_owned(),
            Decision::AllowAlways => i18n::t(locale, "Approve and don't ask again").to_owned(),
            Decision::Deny => i18n::t(locale, "Deny").to_owned(),
            Decision::Other => {
                i18n::t_slots(locale, "Option {index}", &[("index", &index.to_string())])
            }
        }
    }

    /// Read the meaning off an answer's wording.
    fn classify(label: &str) -> Decision {
        let text = label.trim().to_ascii_lowercase();
        let starts = |word: &str| {
            text == word
                || text.starts_with(&format!("{word} "))
                || text.starts_with(&format!("{word},"))
        };
        // Every way an agent has been seen to say "and stop asking". The
        // contractions were here from the start; the spelled-out forms were
        // not, so `Yes, and do not ask again` -- which Claude Code writes
        // whenever its style setting prefers full words -- was read as a
        // one-off Allow, and the reader who chose "stop asking" was asked
        // again.
        let permanent = [
            "don't ask",
            "don’t ask",
            "dont ask",
            "do not ask",
            "never ask",
            "stop asking",
            "always",
            "remember this",
            "remember my",
            "this session",
            "all edits",
            "auto-accept",
            "auto accept",
        ]
        .iter()
        .any(|needle| text.contains(needle));
        if starts("yes")
            || starts("allow")
            || starts("approve")
            || starts("proceed")
            || starts("accept")
        {
            return if permanent {
                Decision::AllowAlways
            } else {
                Decision::Allow
            };
        }
        if starts("no")
            || starts("deny")
            || starts("reject")
            || starts("cancel")
            || starts("decline")
            || starts("exit")
            || starts("stop")
        {
            return Decision::Deny;
        }
        Decision::Other
    }
}

/// One answer the agent offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalOption {
    /// The number the agent printed, which is also the digit that selects it.
    pub index: u32,
    /// The answer verbatim.
    pub label: String,
    /// Whether the agent's cursor is on this answer.
    pub selected: bool,
    pub decision: Decision,
}

/// One answer as a push notification is allowed to know it.
///
/// A push is built in a background watcher and worded once per language among
/// the registered devices, so what the menu *offers* has to outlive the moment
/// the wording is chosen. It is also the whole of what may travel: an index and
/// a decision, neither of which the agent wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushChoice {
    pub index: u32,
    pub decision: Decision,
}

/// The option summary a push notification may carry: indices and decisions
/// only, with labels the gateway wrote itself, in the reader's language.
pub fn push_options(choices: &[PushChoice], locale: Locale) -> Vec<Value> {
    choices
        .iter()
        .map(|choice| {
            json!({
                "index": choice.index,
                "decision": choice.decision.as_str(),
                "label": choice.decision.public_label(locale, choice.index),
            })
        })
        .collect()
}

/// A permission menu the agent is blocked on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// The question, verbatim.
    pub prompt: String,
    /// The lines the agent drew around the question: what tool, what command,
    /// what file. Verbatim and capped.
    pub context: Vec<String>,
    /// The tool name, when the agent named one in a `Tool(input)` line.
    pub tool: Option<String>,
    /// The key-hint footer, when the agent drew one.
    pub hint: Option<String>,
    pub options: Vec<ApprovalOption>,
    /// Row span of the menu in the source text, so a client can correlate with
    /// the raw terminal view -- the same discipline `parts` uses.
    pub start: usize,
    pub end: usize,
    /// Stable identity of *this* question with *these* answers. Two reads of an
    /// unanswered menu agree on it; the next menu does not. It is what makes
    /// answering idempotent and what tells a resolved menu from a replaced one.
    pub fingerprint: String,
}

impl Approval {
    pub fn to_json(&self) -> Value {
        json!({
            "fingerprint": self.fingerprint,
            "prompt": self.prompt,
            "tool": self.tool,
            "context": self.context,
            "hint": self.hint,
            "options": self
                .options
                .iter()
                .map(|option| json!({
                    "index": option.index,
                    "label": option.label,
                    "selected": option.selected,
                    "decision": option.decision.as_str(),
                }))
                .collect::<Vec<_>>(),
            "range": { "start": self.start, "end": self.end },
        })
    }

    /// What this menu offers, with everything the agent wrote stripped off.
    ///
    /// The privacy rule for notifications is that they say *that* something
    /// needs an answer, never *what*. An agent's own wording routinely quotes a
    /// command or a path ("don't ask again for: npm install *"), so it is the
    /// one thing that must not be forwarded. An index and a decision carry no
    /// terminal content, which is why a push may hold on to them long after the
    /// `Approval` they came from has been dropped.
    pub fn push_choices(&self) -> Vec<PushChoice> {
        self.options
            .iter()
            .map(|option| PushChoice {
                index: option.index,
                decision: option.decision,
            })
            .collect()
    }

    /// The option a named decision resolves to, preferring the lowest-numbered
    /// match so "allow" means the plain one-time allow rather than a blanket
    /// one.
    pub fn option_for(&self, decision: Decision) -> Option<&ApprovalOption> {
        self.options
            .iter()
            .find(|option| option.decision == decision)
    }

    pub fn option(&self, index: u32) -> Option<&ApprovalOption> {
        self.options.iter().find(|option| option.index == index)
    }

    /// Keys that answer the menu with `index`.
    ///
    /// Agents accept the digit itself as a selection, which is one keystroke and
    /// cannot land on the wrong row if the cursor moved between the read and the
    /// answer. Past nine digits stop being unambiguous, so the cursor is walked
    /// instead. Either way the caller follows up with a deferred `Enter` when
    /// the menu is still standing, because a menu that wants a confirm and a
    /// menu that acts on the digit look identical from outside.
    pub fn keys_for(&self, index: u32) -> Option<Vec<String>> {
        let target = self
            .options
            .iter()
            .position(|option| option.index == index)?;
        if index <= MAX_DIGIT_OPTION {
            return Some(vec![index.to_string()]);
        }
        let cursor = self
            .options
            .iter()
            .position(|option| option.selected)
            .unwrap_or(0);
        let mut keys = Vec::new();
        let (key, steps) = if target >= cursor {
            ("Down", target - cursor)
        } else {
            ("Up", cursor - target)
        };
        for _ in 0..steps {
            keys.push(key.to_string());
        }
        keys.push("Enter".into());
        Some(keys)
    }
}

/// Find the permission menu a pane is blocked on, if it is blocked on one.
///
/// `text` is the pane's plain text, newest last -- what `pane.read` returns for
/// `visible` or `recent_unwrapped`.
pub fn detect(text: &str) -> Option<Approval> {
    let lines: Vec<&str> = text.lines().collect();
    let floor = lines.len().saturating_sub(TAIL_LINES);

    // Walk up from the end. Blank lines and key hints may sit under the menu;
    // anything else means the pane has already moved past it.
    let mut row = lines.len();
    let mut hint = None;
    let mut hints = 0;
    while row > floor {
        let line = lines[row - 1];
        if line.trim().is_empty() {
            row -= 1;
            continue;
        }
        if parse_option(line).is_some() {
            break;
        }
        if hints < MAX_TRAILING_HINTS && is_hint(line) {
            hints += 1;
            hint = Some(line.trim().to_string());
            row -= 1;
            continue;
        }
        return None;
    }

    let last = row.checked_sub(1)?;
    let mut options = Vec::new();
    while row > floor {
        match parse_option(lines[row - 1]) {
            Some(option) => {
                options.push(option);
                row -= 1;
                if options.len() > MAX_OPTIONS {
                    return None;
                }
            }
            None => break,
        }
    }
    options.reverse();
    if options.len() < 2 {
        return None;
    }
    // The numbers must actually enumerate. A menu is `1..n`; a coincidence is
    // not.
    if options
        .iter()
        .enumerate()
        .any(|(position, option)| option.index as usize != position + 1)
    {
        return None;
    }
    if options.iter().filter(|option| option.selected).count() > 1 {
        return None;
    }
    let first = row;

    // The question is the nearest line above that reads like one.
    let mut question_row = None;
    let mut fallback_row = None;
    let mut seen = 0;
    let mut probe = first;
    while probe > floor && seen < QUESTION_LOOKBACK {
        probe -= 1;
        let line = lines[probe];
        if line.trim().is_empty() {
            continue;
        }
        seen += 1;
        if fallback_row.is_none() {
            fallback_row = Some(probe);
        }
        if line.contains('?') {
            question_row = Some(probe);
            break;
        }
    }
    let question_row = question_row.or(fallback_row)?;
    let prompt = lines[question_row].trim().to_string();

    // Either the question asks something, or the answers answer something. A
    // numbered list under a heading is not a permission menu.
    let yes_no = options.iter().any(|option| {
        matches!(
            option.decision,
            Decision::Allow | Decision::AllowAlways | Decision::Deny
        )
    });
    if !prompt.contains('?') && !yes_no {
        return None;
    }
    // And it must be drawn like a menu: a cursor, or the key hints that tell the
    // user how to answer.
    if hint.is_none() && !options.iter().any(|option| option.selected) {
        return None;
    }

    let (context, block_start) = collect_context(&lines, question_row, first, last, floor);
    let tool = find_tool(&lines, block_start, floor, &context);
    let fingerprint = fingerprint(&prompt, &options);

    Some(Approval {
        prompt,
        context,
        tool,
        hint,
        options,
        start: block_start,
        end: last,
        fingerprint,
    })
}

/// `❯ 2. Yes, and don't ask again` -> option 2, selected.
fn parse_option(line: &str) -> Option<ApprovalOption> {
    let mut rest = line.trim_start();
    let mut selected = false;
    if let Some(glyph) = rest.chars().next() {
        if CURSOR_GLYPHS.contains(&glyph) {
            let after = &rest[glyph.len_utf8()..];
            // A glyph only marks when it is followed by a space, the same rule
            // the marker dictionaries use: `>3.` in output is not a cursor.
            if after.starts_with(' ') {
                selected = true;
                rest = after.trim_start();
            }
        }
    }
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = &rest[digits.len()..];
    let rest = rest.strip_prefix('.')?;
    if !rest.starts_with(' ') {
        return None;
    }
    let label = rest.trim();
    if label.is_empty() {
        return None;
    }
    let index = digits.parse().ok()?;
    if index == 0 {
        return None;
    }
    Some(ApprovalOption {
        index,
        label: label.to_string(),
        selected,
        decision: Decision::classify(label),
    })
}

fn is_hint(line: &str) -> bool {
    let text = line.trim().to_ascii_lowercase();
    if text.is_empty() || text.chars().count() > 160 {
        return false;
    }
    HINT_MARKERS.iter().any(|marker| text.contains(marker))
        || text.starts_with("esc ")
        || text.starts_with("enter ")
        || text.starts_with("tab ")
}

/// Is this line a drawn rule, and is it the kind that ends a menu region?
fn rule(line: &str) -> Option<bool> {
    let text = line.trim();
    if text.chars().count() < 8 {
        return None;
    }
    if text.chars().all(|glyph| HARD_RULE_CHARS.contains(&glyph)) {
        return Some(true);
    }
    if text.chars().all(|glyph| SOFT_RULE_CHARS.contains(&glyph)) {
        return Some(false);
    }
    None
}

fn opens_transcript(line: &str) -> bool {
    let mut chars = line.trim_start().chars();
    match (chars.next(), chars.next()) {
        (Some(glyph), Some(' ')) => TRANSCRIPT_GLYPHS.contains(&glyph),
        _ => false,
    }
}

/// The lines the agent drew around the question, in source order, plus the row
/// the menu region starts on.
fn collect_context(
    lines: &[&str],
    question_row: usize,
    first_option: usize,
    last_option: usize,
    floor: usize,
) -> (Vec<String>, usize) {
    let mut above = Vec::new();
    let mut block_start = question_row;
    let mut probe = question_row;
    while probe > floor && above.len() < MAX_CONTEXT_LINES {
        probe -= 1;
        let line = lines[probe];
        match rule(line) {
            // A hard rule opens the menu region; the transcript above it is not
            // part of this request.
            Some(true) => {
                block_start = probe;
                break;
            }
            // A soft rule brackets a preview inside the menu.
            Some(false) => {
                block_start = probe;
                continue;
            }
            None => {}
        }
        if opens_transcript(line) {
            block_start = probe + 1;
            break;
        }
        block_start = probe;
        if line.trim().is_empty() {
            continue;
        }
        above.push(line.trim().to_string());
    }
    above.reverse();

    // Lines between the question and the answers belong to the request too.
    for line in lines.iter().take(first_option).skip(question_row + 1) {
        if above.len() >= MAX_CONTEXT_LINES {
            break;
        }
        if line.trim().is_empty() || rule(line).is_some() {
            continue;
        }
        above.push(line.trim().to_string());
    }
    let _ = last_option;

    let mut budget = MAX_CONTEXT_CHARS;
    let context = above
        .into_iter()
        .take_while(|line| {
            let fits = budget > 0;
            budget = budget.saturating_sub(line.chars().count());
            fits
        })
        .collect();
    (context, block_start)
}

/// The tool the request is about, read off the nearest `Tool(input)` line the
/// agent drew -- inside the menu first, then in the transcript above it.
fn find_tool(
    lines: &[&str],
    block_start: usize,
    floor: usize,
    context: &[String],
) -> Option<String> {
    for line in context {
        if let Some(name) = tool_name(line) {
            return Some(name);
        }
    }
    let stop = block_start.saturating_sub(TOOL_LOOKBACK).max(floor);
    let mut probe = block_start;
    while probe > stop {
        probe -= 1;
        if let Some(name) = tool_name(lines[probe]) {
            return Some(name);
        }
    }
    None
}

/// `⏺ Bash(npm install)` -> `Bash`. One identifier, immediately followed by an
/// opening paren -- the same shape `parts` requires of a tool block, so a
/// sentence that happens to contain a paren is not read as a tool.
fn tool_name(line: &str) -> Option<String> {
    let mut rest = line.trim_start();
    if let Some(glyph) = rest.chars().next() {
        if TRANSCRIPT_GLYPHS.contains(&glyph) {
            rest = rest[glyph.len_utf8()..].trim_start();
        }
    }
    let name: String = rest
        .chars()
        .take_while(|glyph| glyph.is_ascii_alphanumeric() || *glyph == '_')
        .collect();
    if name.is_empty() || !name.starts_with(|glyph: char| glyph.is_ascii_alphabetic()) {
        return None;
    }
    if !rest[name.len()..].starts_with('(') {
        return None;
    }
    Some(name)
}

fn fingerprint(prompt: &str, options: &[ApprovalOption]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    for option in options {
        hasher.update([0x1f]);
        hasher.update(option.index.to_string().as_bytes());
        hasher.update([0x1e]);
        hasher.update(option.label.as_bytes());
    }
    format!("{:x}", hasher.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASH: &str = include_str!("../tests/fixtures/approval-claude-bash.txt");
    const READ: &str = include_str!("../tests/fixtures/approval-claude-read.txt");
    const WRITE: &str = include_str!("../tests/fixtures/approval-claude-write.txt");
    const FETCH: &str = include_str!("../tests/fixtures/approval-claude-fetch.txt");
    const TRUST: &str = include_str!("../tests/fixtures/approval-claude-trust.txt");
    const APIKEY: &str = include_str!("../tests/fixtures/approval-claude-apikey.txt");
    const RESOLVED: &str = include_str!("../tests/fixtures/approval-claude-resolved.txt");
    const TRANSCRIPT: &str = include_str!("../tests/fixtures/claude-transcript.txt");
    const QODER: &str = include_str!("../tests/fixtures/qoder-transcript.txt");

    fn labels(approval: &Approval) -> Vec<&str> {
        approval
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect()
    }

    /// The wording an agent chooses must not decide whether "stop asking"
    /// means it. Contractions were covered from the start; the spelled-out
    /// forms were not, and read as a one-off Allow.
    #[test]
    fn every_way_of_saying_stop_asking_is_permanent() {
        for label in [
            "Yes, and don't ask again",
            "Yes, and don’t ask again",
            "Yes, and dont ask again",
            "Yes, and do not ask again",
            "Yes, and never ask again",
            "Yes, and stop asking",
            "Yes, always",
            "Yes, and remember this choice",
            "Yes, and remember my answer",
            "Yes, allow all edits this session",
            "Yes, auto-accept edits",
        ] {
            assert_eq!(
                Decision::classify(label),
                Decision::AllowAlways,
                "{label:?} means stop asking"
            );
        }
        // A plain yes is still a one-off, and a no is still a no however it
        // is worded.
        assert_eq!(Decision::classify("Yes"), Decision::Allow);
        assert_eq!(Decision::classify("Yes, proceed"), Decision::Allow);
        assert_eq!(
            Decision::classify("No, and do not ask again"),
            Decision::Deny
        );
    }

    fn decisions(approval: &Approval) -> Vec<&'static str> {
        approval
            .options
            .iter()
            .map(|option| option.decision.as_str())
            .collect()
    }

    #[test]
    fn a_bash_command_menu_carries_its_question_answers_and_command() {
        let approval = detect(BASH).expect("the pane is blocked on a bash approval");
        assert_eq!(approval.prompt, "Do you want to proceed?");
        assert_eq!(
            labels(&approval),
            ["Yes", "Yes, and don’t ask again for: npm install *", "No"]
        );
        assert_eq!(decisions(&approval), ["allow", "allow_always", "deny"]);
        assert!(approval.options[0].selected);
        assert_eq!(approval.tool.as_deref(), Some("Bash"));
        // The agent's framing of the request travels with it, so the phone can
        // show what is being asked without a second read.
        assert!(approval
            .context
            .iter()
            .any(|line| line == "npm install --save-dev vitest"));
        assert!(approval
            .context
            .iter()
            .any(|line| line == "This command requires approval"));
        assert_eq!(
            approval.hint.as_deref(),
            Some("Esc to cancel · Tab to amend · ctrl+e to explain")
        );
    }

    #[test]
    fn a_file_read_menu_names_the_tool_from_the_menu_itself() {
        let approval = detect(READ).expect("the pane is blocked on a read approval");
        assert_eq!(approval.prompt, "Do you want to proceed?");
        assert_eq!(approval.tool.as_deref(), Some("Read"));
        assert_eq!(
            decisions(&approval),
            ["allow", "allow_always", "deny"],
            "a session-wide allow is not a plain allow"
        );
        assert_eq!(
            approval.options[1].label,
            "Yes, allow reading from etc/ during this session"
        );
    }

    #[test]
    fn a_write_menu_survives_the_diff_preview_drawn_inside_it() {
        // The preview is bracketed by its own rules and numbered lines. Neither
        // may be mistaken for the menu's own answers, and neither may hide the
        // heading above them.
        let approval = detect(WRITE).expect("the pane is blocked on a write approval");
        assert_eq!(approval.prompt, "Do you want to create notes.md?");
        assert_eq!(labels(&approval).len(), 3);
        assert_eq!(
            decisions(&approval),
            ["allow", "allow_always", "deny"],
            "allow all edits this session is a blanket allow"
        );
        assert!(approval.context.iter().any(|line| line == "Create file"));
    }

    #[test]
    fn a_web_fetch_menu_reads_a_host_scoped_always_allow() {
        let approval = detect(FETCH).expect("the pane is blocked on a fetch approval");
        assert_eq!(
            approval.prompt,
            "Do you want to allow Claude to fetch this content?"
        );
        assert_eq!(decisions(&approval), ["allow", "allow_always", "deny"]);
        assert_eq!(approval.tool.as_deref(), Some("Fetch"));
    }

    #[test]
    fn a_startup_trust_menu_is_an_approval_even_with_no_tool_behind_it() {
        let approval = detect(TRUST).expect("the pane is blocked on the trust prompt");
        assert!(approval
            .prompt
            .contains("Is this a project you created or one you trust?"));
        assert_eq!(labels(&approval), ["Yes, I trust this folder", "No, exit"]);
        assert_eq!(decisions(&approval), ["allow", "deny"]);
        assert_eq!(approval.tool, None);
        assert_eq!(
            approval.hint.as_deref(),
            Some("Enter to confirm · Esc to cancel")
        );
    }

    #[test]
    fn the_cursor_is_reported_wherever_the_agent_put_it() {
        // This menu opens with the cursor on the second answer, so a client that
        // assumed "the first one is selected" would show the wrong default.
        let approval = detect(APIKEY).expect("the pane is blocked on the api key prompt");
        assert_eq!(approval.prompt, "Do you want to use this API key?");
        assert!(!approval.options[0].selected);
        assert!(approval.options[1].selected);
        assert_eq!(decisions(&approval), ["allow", "deny"]);
    }

    #[test]
    fn an_answered_menu_is_no_longer_pending() {
        // The agent redraws the menu away once it is answered; what is left is
        // the composer. Reading that as a pending approval would have the phone
        // offer to answer a question nobody asked.
        assert!(detect(RESOLVED).is_none());
    }

    #[test]
    fn ordinary_agent_transcripts_hold_no_approvals() {
        assert!(detect(TRANSCRIPT).is_none());
        assert!(detect(QODER).is_none());
    }

    #[test]
    fn a_numbered_list_in_output_is_not_a_menu() {
        let text =
            "Here are the steps:\n 1. Install the toolchain\n 2. Run the tests\n 3. Ship it\n";
        assert!(detect(text).is_none(), "no question, no answers, no cursor");
    }

    #[test]
    fn a_menu_that_scrolled_away_is_not_pending() {
        let text = concat!(
            " Do you want to proceed?\n",
            " ❯ 1. Yes\n",
            "   2. No\n",
            "\n",
            "⏺ Bash(cargo test)\n",
            "  ⎿  test result: ok\n",
        );
        assert!(detect(text).is_none());
    }

    #[test]
    fn options_must_enumerate_from_one() {
        let text = " Do you want to proceed?\n ❯ 2. Yes\n   4. No\n\n Esc to cancel\n";
        assert!(detect(text).is_none());
    }

    #[test]
    fn a_single_answer_is_not_a_choice() {
        let text = " Do you want to proceed?\n ❯ 1. Yes\n\n Esc to cancel\n";
        assert!(detect(text).is_none());
    }

    #[test]
    fn the_fingerprint_pins_the_question_and_its_answers() {
        let bash = detect(BASH).unwrap();
        let again = detect(BASH).unwrap();
        assert_eq!(bash.fingerprint, again.fingerprint);
        assert_ne!(bash.fingerprint, detect(READ).unwrap().fingerprint);
        assert_eq!(bash.fingerprint.len(), 16);
    }

    #[test]
    fn a_decision_resolves_to_the_narrowest_option_that_means_it() {
        let approval = detect(BASH).unwrap();
        assert_eq!(approval.option_for(Decision::Allow).unwrap().index, 1);
        assert_eq!(approval.option_for(Decision::AllowAlways).unwrap().index, 2);
        assert_eq!(approval.option_for(Decision::Deny).unwrap().index, 3);
    }

    #[test]
    fn answering_is_one_digit_while_the_digits_are_unambiguous() {
        let approval = detect(BASH).unwrap();
        assert_eq!(approval.keys_for(3).unwrap(), ["3"]);
        assert_eq!(approval.keys_for(1).unwrap(), ["1"]);
        assert!(approval.keys_for(9).is_none(), "there is no ninth answer");
    }

    #[test]
    fn a_long_menu_walks_the_cursor_instead_of_typing_a_number() {
        let mut text = String::from(" Do you want to proceed?\n");
        for index in 1..=11 {
            let cursor = if index == 2 { "❯" } else { " " };
            text.push_str(&format!("{cursor} {index}. Option {index}\n"));
        }
        text.push_str("\n Esc to cancel\n");
        let approval = detect(&text).expect("eleven answers is still a menu");
        assert_eq!(approval.options.len(), 11);
        // Answers one through nine are still one keystroke; only the ones a
        // digit cannot name walk the cursor from wherever the agent left it.
        assert_eq!(approval.keys_for(4).unwrap(), ["4"]);
        assert_eq!(
            approval.keys_for(11).unwrap(),
            ["Down", "Down", "Down", "Down", "Down", "Down", "Down", "Down", "Down", "Enter"]
        );
    }

    #[test]
    fn a_push_payload_names_the_choices_without_quoting_the_terminal() {
        // The whole privacy rule for notifications lives in this assertion: the
        // agent quoted a command in its own label, and none of it may leave.
        let approval = detect(BASH).unwrap();
        let options = push_options(&approval.push_choices(), Locale::En);
        let rendered = serde_json::to_string(&options).unwrap();
        assert!(!rendered.contains("npm"));
        assert!(!rendered.contains("vitest"));
        assert_eq!(options[0]["label"], "Approve");
        assert_eq!(options[1]["label"], "Approve and don't ask again");
        assert_eq!(options[2]["label"], "Deny");
        assert_eq!(options[2]["index"], 3);
    }

    #[test]
    fn a_translated_push_payload_still_quotes_none_of_the_terminal() {
        // Translating the gateway's own four labels cannot weaken the rule
        // above, because the rule is about whose words they are and not about
        // which language they are in. The same fixture, the same command in the
        // agent's own label, and the same nothing on the wire.
        let approval = detect(BASH).unwrap();
        let options = push_options(&approval.push_choices(), Locale::ZhTw);
        let rendered = serde_json::to_string(&options).unwrap();
        assert!(!rendered.contains("npm"));
        assert!(!rendered.contains("vitest"));
        assert_eq!(options[0]["label"], "核准");
        assert_eq!(options[1]["label"], "核准，且不再詢問");
        assert_eq!(options[2]["label"], "拒絕");
    }

    #[test]
    fn the_decision_vocabulary_is_byte_identical_in_every_locale() {
        // `decision` and `index` are what a client dispatches on; `label` is
        // what a person reads. Only the second one has a language.
        let approval = detect(BASH).unwrap();
        let english = push_options(&approval.push_choices(), Locale::En);
        let chinese = push_options(&approval.push_choices(), Locale::ZhTw);
        assert_eq!(english.len(), chinese.len());
        for (english, chinese) in english.iter().zip(&chinese) {
            assert_eq!(english["decision"], chinese["decision"]);
            assert_eq!(english["index"], chinese["index"]);
            assert_ne!(english["label"], chinese["label"]);
        }
        assert_eq!(
            Decision::Other.public_label(Locale::ZhTw, 7),
            "選項 7",
            "an index is a number in every language"
        );
    }

    #[test]
    fn the_json_shape_is_the_one_a_client_dispatches_on() {
        let value = detect(BASH).unwrap().to_json();
        assert!(value["fingerprint"].is_string());
        assert_eq!(value["tool"], "Bash");
        assert_eq!(value["options"][2]["decision"], "deny");
        assert_eq!(value["options"][0]["selected"], true);
        assert!(value["range"]["start"].is_u64());
        assert!(value["range"]["end"].is_u64());
    }
}
