//! What the composer's text and caret say, decided from nothing else -- the
//! App's rule (`file-mentions.ts`, `composer-mentions.ts`, `agent-commands.ts`):
//! no keystroke history and no "the user pressed @" flag, so a paste, an undo
//! or a caret moved back into an old word all resolve like fresh typing.
//!
//! Positions are character indices into the draft (what the text area's
//! columns count), not bytes.

use muqun_gateway::agents::{CommandInfo, SkillInfo};

/// Past this many characters the word stopped being a file query (the App's
/// `MAX_FILE_MENTION_QUERY`).
pub const MAX_FILE_QUERY: usize = 96;
/// How many rows a popup shows at once.
pub const VISIBLE_ROWS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `@path` anywhere, at the start of a word.
    File,
    /// `/command` at the very start of the draft.
    Command,
    /// `$skill` at the very start of the draft.
    Skill,
}

/// The word the caret is inside, as a span of the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: Kind,
    /// Index of the trigger character.
    pub start: usize,
    /// Index just past the query: the caret.
    pub end: usize,
    /// What was typed after the trigger character.
    pub query: String,
}

/// The trigger the caret is in, or `None`.
pub fn find(text: &str, caret: usize) -> Option<Trigger> {
    let chars: Vec<char> = text.chars().collect();
    let end = caret.min(chars.len());
    if end == 0 {
        return None;
    }
    // Walk back through the current word; whitespace means the caret is in
    // no word at all. The first trigger character met from the start of the
    // word decides.
    let mut word_start = end;
    while word_start > 0 {
        if chars[word_start - 1].is_whitespace() {
            break;
        }
        word_start -= 1;
    }
    if word_start == end {
        return None;
    }
    let lead = chars[word_start];
    let query: String = chars[word_start + 1..end].iter().collect();
    let kind = match lead {
        '@' if query.chars().count() <= MAX_FILE_QUERY => Kind::File,
        '/' if word_start == 0 => Kind::Command,
        '$' if word_start == 0 => Kind::Skill,
        _ => return None,
    };
    Some(Trigger {
        kind,
        start: word_start,
        end,
        query,
    })
}

/// The draft and caret once `insert` replaces the trigger's span: the text
/// goes in as given plus one separating space, unless whitespace already
/// follows (then the caret steps over it).
pub fn insert(text: &str, trigger: &Trigger, insert: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let start = trigger.start.min(chars.len());
    let end = trigger.end.clamp(start, chars.len());
    let following: String = chars[end..].iter().collect();
    let needs_space = !following.starts_with(' ') && !following.starts_with('\t');
    let inserted = if needs_space {
        format!("{insert} ")
    } else {
        insert.to_string()
    };
    let head: String = chars[..start].iter().collect();
    let caret = start + inserted.chars().count() + usize::from(!needs_space);
    (format!("{head}{inserted}{following}"), caret)
}

/// The character index of (line, column) in the lines joined by `\n`.
pub fn caret_index(lines: &[String], line: usize, column: usize) -> usize {
    let before: usize = lines.iter().take(line).map(|l| l.chars().count() + 1).sum();
    before + column
}

/// The (line, column) of a character index in `text`.
pub fn caret_position(text: &str, index: usize) -> (usize, usize) {
    let mut line = 0;
    let mut column = 0;
    for (i, c) in text.chars().enumerate() {
        if i == index {
            break;
        }
        if c == '\n' {
            line += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    (line, column)
}

// ---------------------------------------------------------------------------
// Slash commands and skills: what a sent line is
// ---------------------------------------------------------------------------

/// What a line the owner sends means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// An ordinary prompt.
    Prompt,
    /// A host command, run through the agent -- never typed to the model.
    Command { name: String, args: String },
    /// A catalogue skill, activated.
    Skill { name: String, args: String },
}

/// The bare name: no slash, no arguments, lower case.
pub fn command_key(name: &str) -> String {
    name.trim()
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

fn split_first_word(body: &str) -> (String, String) {
    match body.find(char::is_whitespace) {
        None => (body.to_lowercase(), String::new()),
        Some(at) => (body[..at].to_lowercase(), body[at..].trim().to_string()),
    }
}

/// `agent-commands.ts` `readSlashCommand` + `readSkillInvocation`: only the
/// host's catalogue can identify a command, so a slash line that matches
/// nothing (`/etc/hosts`) is a prompt. Commands win over slash skills; a
/// `$` line names any skill.
pub fn read_intent(text: &str, commands: &[CommandInfo], skills: &[SkillInfo]) -> Intent {
    let trimmed = text.trim();
    if let Some(body) = trimmed.strip_prefix('/') {
        let (key, args) = split_first_word(body);
        if key.is_empty() {
            return Intent::Prompt;
        }
        if let Some(command) = commands.iter().find(|c| command_key(&c.name) == key) {
            return Intent::Command {
                name: command.name.clone(),
                args,
            };
        }
        if let Some(skill) = skills
            .iter()
            .find(|s| s.slash && s.id.to_lowercase() == key)
        {
            return Intent::Skill {
                name: skill.id.clone(),
                args,
            };
        }
        return Intent::Prompt;
    }
    if let Some(body) = trimmed.strip_prefix('$') {
        let (key, args) = split_first_word(body);
        if key.is_empty() {
            return Intent::Prompt;
        }
        if let Some(skill) = skills.iter().find(|s| s.id.to_lowercase() == key) {
            return Intent::Skill {
                name: skill.id.clone(),
                args,
            };
        }
    }
    Intent::Prompt
}

/// One row of a popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// What replaces the trigger span when the row is picked.
    pub insert: String,
    pub label: String,
    pub detail: String,
}

/// Rows of `/` for a query: the catalogue's commands and its slash skills,
/// filtered by a case-insensitive match on the name (prefix first).
pub fn command_rows(query: &str, commands: &[CommandInfo], skills: &[SkillInfo]) -> Vec<Row> {
    let mut rows: Vec<Row> = commands
        .iter()
        .map(|c| Row {
            insert: format!("/{}", command_key(&c.name)),
            label: format!("/{}", command_key(&c.name)),
            detail: c.description.clone().unwrap_or_default(),
        })
        .collect();
    rows.extend(skills.iter().filter(|s| s.slash).map(|s| Row {
        insert: format!("/{}", s.id),
        label: format!("/{}", s.id),
        detail: skill_detail(s),
    }));
    filter_rows(rows, query)
}

/// Rows of `$` for a query: every skill.
pub fn skill_rows(query: &str, skills: &[SkillInfo]) -> Vec<Row> {
    let rows = skills
        .iter()
        .map(|s| Row {
            insert: format!("${}", s.id),
            label: format!("${}", s.id),
            detail: skill_detail(s),
        })
        .collect();
    filter_rows(rows, query)
}

fn skill_detail(skill: &SkillInfo) -> String {
    if !skill.name.is_empty()
        && !skill.description.is_empty()
        && skill.name.to_lowercase() != skill.id.to_lowercase()
    {
        format!("{} · {}", skill.name, skill.description)
    } else if skill.description.is_empty() {
        skill.name.clone()
    } else {
        skill.description.clone()
    }
}

fn filter_rows(rows: Vec<Row>, query: &str) -> Vec<Row> {
    let wanted = query.trim().to_lowercase();
    if wanted.is_empty() {
        return rows;
    }
    let mut prefix = Vec::new();
    let mut inside = Vec::new();
    for row in rows {
        let name = row.label[1..].to_lowercase();
        if name.starts_with(&wanted) {
            prefix.push(row);
        } else if name.contains(&wanted) || row.detail.to_lowercase().contains(&wanted) {
            inside.push(row);
        }
    }
    prefix.extend(inside);
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str) -> CommandInfo {
        CommandInfo {
            name: name.into(),
            description: Some(format!("{name} does it")),
            mode: None,
            template: None,
        }
    }

    fn skill(id: &str, slash: bool) -> SkillInfo {
        SkillInfo {
            id: id.into(),
            name: id.into(),
            description: "d".into(),
            slash,
            autoinvoke: false,
        }
    }

    #[test]
    fn an_at_opens_a_mention_only_at_a_word_start() {
        let t = find("look at @src/ma", 15).unwrap();
        assert_eq!(
            (t.kind, t.start, t.end, t.query.as_str()),
            (Kind::File, 8, 15, "src/ma")
        );
        assert!(find("mail me@example.com", 19).is_none(), "mid-word");
        assert!(find("pkg@1.2.3", 9).is_none());
        assert!(find("a @x b", 6).is_none(), "the caret is past the word");
        assert_eq!(find("@", 1).unwrap().query, "");
        assert!(find("@", 0).is_none());
        let long = format!("@{}", "a".repeat(MAX_FILE_QUERY + 1));
        assert!(find(&long, long.chars().count()).is_none());
    }

    #[test]
    fn a_slash_or_dollar_opens_only_at_the_start() {
        assert_eq!(find("/rev", 4).unwrap().kind, Kind::Command);
        assert_eq!(find("$sk", 3).unwrap().kind, Kind::Skill);
        assert!(find("see /rev", 8).is_none());
        assert!(find("a/b", 3).is_none());
        assert!(find("/rev now", 8).is_none(), "past the first word");
    }

    #[test]
    fn picking_inserts_the_text_and_one_space() {
        let t = find("hi @sr there", 6).unwrap();
        let (text, caret) = insert("hi @sr there", &t, "src/main.rs");
        assert_eq!(text, "hi src/main.rs there");
        assert_eq!(&text[caret..], "there", "the caret steps over the space");
        let t = find("/co", 3).unwrap();
        assert_eq!(insert("/co", &t, "/commit"), ("/commit ".to_string(), 8));
    }

    #[test]
    fn caret_positions_round_trip() {
        let lines = vec!["ab".to_string(), "cde".to_string()];
        assert_eq!(caret_index(&lines, 1, 2), 5);
        assert_eq!(caret_position("ab\ncde", 5), (1, 2));
        assert_eq!(caret_position("ab\ncde", 2), (0, 2));
    }

    #[test]
    fn only_the_catalogue_identifies_a_command() {
        let commands = [command("/init"), command("review")];
        let skills = [skill("deploy", true), skill("quiet", false)];
        assert_eq!(
            read_intent("/etc/hosts", &commands, &skills),
            Intent::Prompt
        );
        assert_eq!(read_intent("/", &commands, &skills), Intent::Prompt);
        assert_eq!(
            read_intent("/REVIEW the diff", &commands, &skills),
            Intent::Command {
                name: "review".into(),
                args: "the diff".into()
            }
        );
        assert_eq!(
            read_intent("/deploy now", &commands, &skills),
            Intent::Skill {
                name: "deploy".into(),
                args: "now".into()
            }
        );
        assert_eq!(read_intent("/quiet", &commands, &skills), Intent::Prompt);
        assert_eq!(
            read_intent("$quiet", &commands, &skills),
            Intent::Skill {
                name: "quiet".into(),
                args: String::new()
            }
        );
        assert_eq!(read_intent("$nothing", &commands, &skills), Intent::Prompt);
        assert_eq!(
            read_intent("plain text", &commands, &skills),
            Intent::Prompt
        );
    }

    #[test]
    fn rows_filter_by_prefix_then_substring() {
        let commands = [command("compact"), command("export")];
        let skills = [skill("commit", true)];
        let rows = command_rows("co", &commands, &skills);
        assert_eq!(
            rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
            ["/compact", "/commit"]
        );
        assert_eq!(command_rows("", &commands, &skills).len(), 3);
        assert_eq!(skill_rows("x", &[skill("a", false)]).len(), 0);
    }
}
