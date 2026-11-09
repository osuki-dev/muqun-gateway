//! What of a timeline is searchable, how a query is read, and the snippet a
//! hit is shown with. Pure functions: no store, no agent.

use serde_json::Value;

use crate::agents::domain::{AgentPart, TimelineItem, TimelineRole, ToolCall};

/// One searchable row: a user prompt, an assistant reply, or a tool call's
/// headline (its command, path or pattern -- never its output).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub item_id: String,
    pub message_id: String,
    pub role: &'static str,
    pub kind: &'static str,
    pub time_ms: u64,
    pub text: String,
}

/// A row's text is cut here: a reply longer than this is a document, and
/// the part a reader remembers is near its start.
const MAX_ROW_CHARS: usize = 16_000;
/// One headline value (a command, a path list) is cut here.
const MAX_HEADLINE_CHARS: usize = 300;

/// The input fields a tool call's headline is read from, in order. A tool
/// call's output and file contents are never indexed.
const HEADLINE_KEYS: [&str; 12] = [
    "command",
    "cmd",
    "file_path",
    "filePath",
    "notebook_path",
    "path",
    "paths",
    "pattern",
    "query",
    "url",
    "glob",
    "description",
];

/// The searchable rows of `items`, already served through the transcript
/// hiding rules. `fallback_ms` dates a row whose item carries no time.
pub(crate) fn rows(items: &[TimelineItem], fallback_ms: u64) -> Vec<Row> {
    items
        .iter()
        .filter_map(|item| {
            let (role, kind, text) = match (&item.role, &item.part) {
                (TimelineRole::User, AgentPart::Text { text }) => ("user", "text", text.clone()),
                (TimelineRole::Assistant, AgentPart::Text { text }) => {
                    ("assistant", "text", text.clone())
                }
                (_, AgentPart::Tool(call)) => ("assistant", "tool", headline(call)?),
                (_, AgentPart::Shell { command, .. }) => {
                    ("assistant", "tool", format!("shell: {}", cut(command)))
                }
                _ => return None,
            };
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            Some(Row {
                item_id: item.id.clone(),
                message_id: item.message_id.clone(),
                role,
                kind,
                time_ms: if item.updated_ms == 0 {
                    fallback_ms
                } else {
                    item.updated_ms
                },
                text: text.chars().take(MAX_ROW_CHARS).collect(),
            })
        })
        .collect()
}

/// `name: value value`, the first two of [`HEADLINE_KEYS`] the input has.
fn headline(call: &ToolCall) -> Option<String> {
    let mut values: Vec<String> = HEADLINE_KEYS
        .iter()
        .filter_map(|key| match call.input.get(*key)? {
            Value::String(text) => Some(cut(text)),
            Value::Array(list) => Some(cut(&list
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "))),
            _ => None,
        })
        .filter(|value| !value.trim().is_empty())
        .take(2)
        .collect();
    if values.is_empty() {
        if let Some(title) = call.title.as_deref().filter(|t| *t != call.name) {
            values.push(cut(title));
        }
    }
    let name = call.name.trim();
    if name.is_empty() && values.is_empty() {
        return None;
    }
    Some(if values.is_empty() {
        name.to_owned()
    } else {
        format!("{name}: {}", values.join(" "))
    })
}

fn cut(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_HEADLINE_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// A content hash of what a row would be shown as, so an unchanged row is
/// never rewritten (FNV-1a; not a security boundary).
pub(crate) fn row_hash(row: &Row) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [row.role, row.kind, &row.message_id, &row.text] {
        for byte in part.bytes().chain([0xff]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash ^= row.time_ms;
    hash.wrapping_mul(0x0100_0000_01b3) as i64
}

/// The longest query accepted, in characters.
pub(crate) const MAX_QUERY_CHARS: usize = 200;
const MAX_TERMS: usize = 8;

/// A query as the index reads it: whitespace-separated terms, each matched
/// as a case-insensitive substring; every term must match. Terms of three
/// or more characters go through the trigram index, shorter ones (a CJK
/// word is often two) are a substring scan over the same rows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Query {
    pub terms: Vec<String>,
}

impl Query {
    pub(crate) fn parse(text: &str) -> Option<Query> {
        let mut terms: Vec<String> = Vec::new();
        for term in text.split_whitespace() {
            let term = term.trim_matches('"');
            if !term.is_empty() && !terms.iter().any(|t| t == term) {
                terms.push(term.to_owned());
            }
        }
        terms.truncate(MAX_TERMS);
        (!terms.is_empty()).then_some(Query { terms })
    }

    /// Terms long enough for the trigram index.
    pub(crate) fn indexed(&self) -> impl Iterator<Item = &str> {
        self.terms
            .iter()
            .filter(|t| t.chars().count() >= 3)
            .map(String::as_str)
    }

    pub(crate) fn short(&self) -> impl Iterator<Item = &str> {
        self.terms
            .iter()
            .filter(|t| t.chars().count() < 3)
            .map(String::as_str)
    }

    /// The FTS5 expression: every indexed term as a quoted phrase, ANDed.
    pub(crate) fn fts_expression(&self) -> Option<String> {
        let phrases: Vec<String> = self
            .indexed()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        (!phrases.is_empty()).then(|| phrases.join(" AND "))
    }
}

/// A `LIKE` pattern for a substring, with `\` as the escape.
pub(crate) fn like_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('%');
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

const SNIPPET_BEFORE: usize = 40;
const SNIPPET_CHARS: usize = 180;

/// A one-line excerpt of `text` around the first match of a term, and the
/// matched ranges in it as `[start, end)` UTF-16 code unit offsets (what a
/// JavaScript or Swift string indexes by).
pub(crate) fn snippet(text: &str, query: &Query) -> (String, Vec<[usize; 2]>) {
    let chars: Vec<char> = text.chars().collect();
    let folded: Vec<char> = chars.iter().copied().map(fold).collect();
    let terms: Vec<Vec<char>> = query
        .terms
        .iter()
        .map(|t| t.chars().map(fold).collect())
        .collect();
    let first = terms
        .iter()
        .filter_map(|term| find(&folded, term, 0))
        .min()
        .unwrap_or(0);
    let start = first.saturating_sub(SNIPPET_BEFORE);
    // Start at a word boundary when one is near.
    let start = (start..first)
        .find(|&at| at == 0 || chars[at - 1].is_whitespace())
        .unwrap_or(start);
    let end = (start + SNIPPET_CHARS).min(chars.len());
    let mut out: Vec<char> = Vec::new();
    if start > 0 {
        out.push('…');
    }
    // Whitespace runs read as one space; a snippet is one line.
    let mut last_space = false;
    for &c in &chars[start..end] {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    if end < chars.len() {
        out.push('…');
    }
    let out_folded: Vec<char> = out.iter().copied().map(fold).collect();
    let mut ranges: Vec<[usize; 2]> = Vec::new();
    for term in &terms {
        let mut from = 0;
        while let Some(at) = find(&out_folded, term, from) {
            ranges.push([at, at + term.len()]);
            from = at + term.len().max(1);
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<[usize; 2]> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range[0] <= last[1] => last[1] = last[1].max(range[1]),
            _ => merged.push(range),
        }
    }
    // Char offsets to UTF-16 offsets.
    let mut utf16 = Vec::with_capacity(out.len() + 1);
    let mut at = 0;
    for c in &out {
        utf16.push(at);
        at += c.len_utf16();
    }
    utf16.push(at);
    let highlights = merged
        .into_iter()
        .map(|[s, e]| [utf16[s], utf16[e]])
        .collect();
    (out.into_iter().collect(), highlights)
}

fn find(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&at| haystack[at..at + needle.len()] == *needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::TimelineItem;
    use serde_json::json;

    fn item(id: &str, role: TimelineRole, part: AgentPart) -> TimelineItem {
        TimelineItem {
            id: id.into(),
            message_id: format!("m-{id}"),
            role,
            part,
            seq: 0,
            updated_ms: 1_000,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
        }
    }

    fn tool(name: &str, input: Value, output: &str) -> AgentPart {
        AgentPart::Tool(
            serde_json::from_value(json!({
                "id": "c1", "name": name, "input": input, "output": output,
                "state": "completed", "status": "completed",
            }))
            .unwrap(),
        )
    }

    #[test]
    fn prompts_replies_and_tool_headlines_are_rows_and_nothing_else_is() {
        let items = vec![
            item(
                "u",
                TimelineRole::User,
                AgentPart::Text {
                    text: "fix the parser".into(),
                },
            ),
            item(
                "r",
                TimelineRole::Assistant,
                AgentPart::Reasoning {
                    text: "secret thoughts".into(),
                    duration_ms: None,
                },
            ),
            item(
                "a",
                TimelineRole::Assistant,
                AgentPart::Text {
                    text: "Done.".into(),
                },
            ),
            item(
                "t",
                TimelineRole::Assistant,
                tool(
                    "bash",
                    json!({"command": "cargo test -p parser", "timeout": 5}),
                    "OUTPUT BODY",
                ),
            ),
            item(
                "f",
                TimelineRole::Assistant,
                tool("read", json!({"file_path": "/src/lib.rs"}), "FILE CONTENTS"),
            ),
            item(
                "s",
                TimelineRole::System,
                AgentPart::System {
                    text: "system notice".into(),
                    description: None,
                },
            ),
            item(
                "y",
                TimelineRole::User,
                AgentPart::Synthetic {
                    text: "injected".into(),
                    description: None,
                },
            ),
        ];
        let rows = rows(&items, 0);
        let shown: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|r| (r.item_id.as_str(), r.kind, r.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("u", "text", "fix the parser"),
                ("a", "text", "Done."),
                ("t", "tool", "bash: cargo test -p parser"),
                ("f", "tool", "read: /src/lib.rs"),
            ]
        );
        assert!(rows
            .iter()
            .all(|r| !r.text.contains("OUTPUT") && !r.text.contains("CONTENTS")));
    }

    #[test]
    fn snippet_highlights_every_term_case_insensitively_in_utf16_units() {
        let query = Query::parse("Parser 数据").unwrap();
        let (text, highlights) = snippet("😀 the PARSER reads 数据库 rows", &query);
        assert_eq!(text, "😀 the PARSER reads 数据库 rows");
        let units: Vec<u16> = text.encode_utf16().collect();
        let marked: Vec<String> = highlights
            .iter()
            .map(|[s, e]| String::from_utf16(&units[*s..*e]).unwrap())
            .collect();
        assert_eq!(marked, ["PARSER", "数据"]);

        let long = format!("{} needle {}", "x ".repeat(200), "y ".repeat(200));
        let (text, highlights) = snippet(&long, &Query::parse("needle").unwrap());
        assert!(text.starts_with('…') && text.ends_with('…'), "{text}");
        assert_eq!(highlights.len(), 1);
        assert!(text.chars().count() <= SNIPPET_CHARS + 2);
    }
}
