//! Composer `@` / `$` / `/` popup machine — same rules as the App's
//! `lib/composer-popup.ts` and `lib/agent-commands.ts`.
//!
//! `/` is host commands at draft start; `$` is every catalog skill at start;
//! `@` is a word-boundary mention (computers + files). Pure: draft, caret,
//! catalog in → rows out. No fetching.

/// Where a trigger character is allowed to open a popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerAnchor {
    /// Whole-message only (`/` and `$`).
    Start,
    /// After whitespace or at offset 0 (`@`).
    Word,
}

/// One row as the panel draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupRow {
    pub id: String,
    pub label: String,
    pub description: String,
    pub insert: String,
    /// Unreachable computers stay listed but cannot be picked (App parity).
    pub disabled: bool,
}

/// The trigger character and everything typed after it, located in the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupQuery {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub term: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedReason {
    NoCatalog,
    NoQuery,
    Dismissed,
    NoMatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupState {
    Closed {
        reason: ClosedReason,
    },
    Open {
        query: PopupQuery,
        rows: Vec<PopupRow>,
    },
}

impl PopupState {
    pub fn open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }

    pub fn query(&self) -> Option<&PopupQuery> {
        match self {
            Self::Open { query, .. } => Some(query),
            Self::Closed { .. } => None,
        }
    }

    pub fn rows(&self) -> &[PopupRow] {
        match self {
            Self::Open { rows, .. } => rows,
            Self::Closed { .. } => &[],
        }
    }
}

/// How one catalog entry is presented and searched.
pub struct TriggerItem {
    pub row: PopupRow,
    /// Fields matched against the typed term; first is primary (fuzzy), rest
    /// are literal secondary matches once the term is long enough.
    pub search: Vec<String>,
}

pub struct Trigger {
    pub char: char,
    pub anchor: TriggerAnchor,
    pub items: Vec<TriggerItem>,
    pub limit: Option<usize>,
}

pub fn closed(reason: ClosedReason) -> PopupState {
    PopupState::Closed { reason }
}

/// The whole trigger/filter step for one draft + caret + catalog.
pub fn read_popup(
    draft: &str,
    caret: Option<usize>,
    trigger: &Trigger,
    dismissed_at: Option<usize>,
    enabled: bool,
) -> PopupState {
    if !enabled || trigger.items.is_empty() {
        return closed(ClosedReason::NoCatalog);
    }
    let caret = clamp_caret(caret, draft.len());
    let Some(query) = scan_query(draft, caret, trigger.char, trigger.anchor) else {
        return closed(ClosedReason::NoQuery);
    };
    if dismissed_at == Some(query.start) {
        return closed(ClosedReason::Dismissed);
    }

    let mut ranked: Vec<(i32, usize, PopupRow)> = Vec::new();
    for (order, item) in trigger.items.iter().enumerate() {
        let Some(score) = rank_fields(&query.term, &item.search) else {
            continue;
        };
        ranked.push((score, order, item.row.clone()));
    }
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let limit = trigger.limit.unwrap_or(ranked.len());
    let rows: Vec<PopupRow> = ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, row)| row)
        .collect();
    if rows.is_empty() {
        return closed(ClosedReason::NoMatch);
    }
    PopupState::Open { query, rows }
}

/// Remember the query start when Esc dismisses an open popup.
#[allow(dead_code)]
pub fn dismiss(state: &PopupState) -> Option<usize> {
    state.query().map(|q| q.start)
}

/// Put the picked row into the draft in place of the query.
#[allow(dead_code)]
pub fn insert_pick(draft: &str, query: &PopupQuery, row: &PopupRow) -> (String, usize) {
    let head = &draft[..query.start];
    let tail = &draft[query.end..];
    let inserted = if tail.starts_with(' ') {
        row.insert.clone()
    } else {
        format!("{} ", row.insert)
    };
    let caret = head.len() + inserted.len();
    (format!("{head}{inserted}{tail}"), caret)
}

// ---------------------------------------------------------------------------
// Slash / skill send routing (App `agent-commands.ts`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Server { name: String, args: String },
    Skill { name: String, args: String },
}

/// Bare name without slash, lower-cased, first token only.
pub fn command_key(name: &str) -> String {
    name.trim()
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// What a typed `/` line is, or `None` when it is an ordinary prompt.
pub fn read_slash_command(
    text: &str,
    server_commands: &[impl AsRef<str>],
    slash_skills: &[impl AsRef<str>],
) -> Option<SlashCommand> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let (key, args) = split_key_args(rest)?;
    if let Some(name) = server_commands
        .iter()
        .find(|c| command_key(c.as_ref()) == key)
    {
        return Some(SlashCommand::Server {
            name: name.as_ref().to_string(),
            args,
        });
    }
    if let Some(id) = slash_skills
        .iter()
        .find(|s| s.as_ref().eq_ignore_ascii_case(&key))
    {
        return Some(SlashCommand::Skill {
            name: id.as_ref().to_string(),
            args,
        });
    }
    None
}

/// A `$` line naming a catalog skill, or `None` when it is a prompt.
pub fn read_skill_invocation(text: &str, skills: &[impl AsRef<str>]) -> Option<(String, String)> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('$')?;
    let (key, args) = split_key_args(rest)?;
    let id = skills
        .iter()
        .find(|s| s.as_ref().eq_ignore_ascii_case(&key))?;
    Some((id.as_ref().to_string(), args))
}

fn split_key_args(body: &str) -> Option<(String, String)> {
    let space = body.find(char::is_whitespace);
    let key = match space {
        Some(i) => body[..i].to_ascii_lowercase(),
        None => body.to_ascii_lowercase(),
    };
    if key.is_empty() {
        return None;
    }
    let args = match space {
        Some(i) => body[i..].trim().to_string(),
        None => String::new(),
    };
    Some((key, args))
}

// ---------------------------------------------------------------------------
// Scanning + fuzzy rank
// ---------------------------------------------------------------------------

const FIELD_RANK_STEP: i32 = 2000;
const MIN_SECONDARY_TERM: usize = 3;

fn clamp_caret(caret: Option<usize>, length: usize) -> usize {
    caret.unwrap_or(length).min(length)
}

fn is_boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| c.is_whitespace())
}

/// The query the caret is sitting in, if any.
pub fn scan_query(
    draft: &str,
    caret: usize,
    trigger: char,
    anchor: TriggerAnchor,
) -> Option<PopupQuery> {
    let bytes = draft.as_bytes();
    let mut start = caret;
    while start > 0 {
        let prev = draft[..start].chars().next_back()?;
        if is_boundary(Some(prev)) {
            break;
        }
        start -= prev.len_utf8();
    }
    let ch = draft[start..].chars().next()?;
    if ch != trigger {
        return None;
    }
    if anchor == TriggerAnchor::Start && start != 0 {
        return None;
    }
    if anchor == TriggerAnchor::Word && start > 0 {
        let before = draft[..start].chars().next_back();
        if !is_boundary(before) {
            return None;
        }
    }

    let mut end = start + ch.len_utf8();
    while end < draft.len() {
        let next = draft[end..].chars().next()?;
        if is_boundary(Some(next)) {
            break;
        }
        end += next.len_utf8();
    }
    if caret < start || caret > end {
        return None;
    }
    let text = draft[start..end].to_string();
    let term = draft[start + ch.len_utf8()..end].to_string();
    let _ = bytes; // keep UTF-8 awareness explicit
    Some(PopupQuery {
        start,
        end,
        text,
        term,
    })
}

fn rank_fields(term: &str, fields: &[String]) -> Option<i32> {
    let primary = fields.first().map(|s| s.as_str()).unwrap_or("");
    if let Some(score) = fuzzy_score(term, primary) {
        return Some(score);
    }
    if term.len() < MIN_SECONDARY_TERM {
        return None;
    }
    let needle = term.to_ascii_lowercase();
    for (index, field) in fields.iter().skip(1).enumerate() {
        if !field.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        return Some(800 - field.len() as i32 - (index as i32 + 1) * FIELD_RANK_STEP);
    }
    None
}

/// How well `term` matches `text`, or `None` for no match.
pub fn fuzzy_score(term: &str, text: &str) -> Option<i32> {
    let needle = term.to_ascii_lowercase();
    let haystack = text.to_ascii_lowercase();
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    if haystack.starts_with(&needle) {
        return Some(1000 - haystack.len() as i32);
    }
    let bare: String = haystack
        .chars()
        .skip_while(|c| !c.is_ascii_alphanumeric())
        .collect();
    if bare.starts_with(&needle) {
        return Some(900 - haystack.len() as i32);
    }
    if haystack.contains(&needle) {
        return Some(800 - haystack.len() as i32);
    }

    let mut score = 700 - haystack.len() as i32;
    let hay: Vec<char> = haystack.chars().collect();
    let mut at = 0usize;
    for character in needle.chars() {
        let found = hay[at..]
            .iter()
            .position(|&c| c == character)
            .map(|i| at + i)?;
        if found == 0
            || hay
                .get(found - 1)
                .is_some_and(|c| !c.is_ascii_alphanumeric())
        {
            score += 8;
        } else if found == at {
            score += 4;
        }
        at = found + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, insert: &str) -> PopupRow {
        PopupRow {
            id: id.into(),
            label: insert.into(),
            description: String::new(),
            insert: insert.into(),
            disabled: false,
        }
    }

    #[test]
    fn slash_is_start_only() {
        let q = scan_query("say /help", 9, '/', TriggerAnchor::Start);
        assert!(q.is_none());
        let q = scan_query("/help", 5, '/', TriggerAnchor::Start).unwrap();
        assert_eq!(q.term, "help");
    }

    #[test]
    fn at_needs_word_boundary() {
        assert!(scan_query("a@b", 3, '@', TriggerAnchor::Word).is_none());
        let q = scan_query("hi @fi", 6, '@', TriggerAnchor::Word).unwrap();
        assert_eq!(q.term, "fi");
        assert_eq!(q.start, 3);
    }

    #[test]
    fn dollar_skills_at_start() {
        let q = scan_query("$review now", 7, '$', TriggerAnchor::Start).unwrap();
        assert_eq!(q.term, "review");
    }

    #[test]
    fn insert_adds_one_space() {
        let query = PopupQuery {
            start: 0,
            end: 5,
            text: "/help".into(),
            term: "help".into(),
        };
        let (draft, caret) = insert_pick("/help", &query, &row("help", "/help"));
        assert_eq!(draft, "/help ");
        assert_eq!(caret, 6);
    }

    #[test]
    fn unknown_slash_is_prompt() {
        let none: &[&str] = &[];
        assert!(read_slash_command("/etc/hosts", none, none).is_none());
        assert_eq!(
            read_slash_command("/CLEAR x", &["/clear"], none),
            Some(SlashCommand::Server {
                name: "/clear".into(),
                args: "x".into(),
            })
        );
        assert_eq!(
            read_skill_invocation("$clear review", &["clear"]),
            Some(("clear".into(), "review".into()))
        );
    }

    #[test]
    fn fuzzy_prefers_prefix() {
        assert!(fuzzy_score("co", "compact").unwrap() > fuzzy_score("co", "recode").unwrap());
    }

    #[test]
    fn popup_filters_and_limits() {
        let trigger = Trigger {
            char: '/',
            anchor: TriggerAnchor::Start,
            items: vec![
                TriggerItem {
                    row: row("a", "/alpha"),
                    search: vec!["/alpha".into(), "first".into()],
                },
                TriggerItem {
                    row: row("b", "/beta"),
                    search: vec!["/beta".into(), "second".into()],
                },
            ],
            limit: Some(10),
        };
        let state = read_popup("/alp", Some(4), &trigger, None, true);
        assert!(state.open());
        assert_eq!(state.rows().len(), 1);
        assert_eq!(state.rows()[0].id, "a");
    }
}
