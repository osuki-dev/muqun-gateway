//! Mention tokens in a composer draft — App `lib/composer-mentions.ts`.
//!
//! Computers are `@name`; files are the inserted path. Ranges drive atomic
//! backspace for typed mentions that are not yet pills.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionKind {
    Computer,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken {
    pub text: String,
    pub kind: MentionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MentionRange {
    pub start: usize,
    pub end: usize,
    pub kind: MentionKind,
}

fn stands_alone(text: &str, start: usize, end: usize) -> bool {
    let before_ok = start == 0
        || text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_whitespace());
    let after_ok = end >= text.len()
        || text[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace());
    before_ok && after_ok
}

/// Every standing occurrence of a token; longest wins overlaps; sorted.
pub fn mention_ranges(text: &str, tokens: &[MentionToken]) -> Vec<MentionRange> {
    if text.is_empty() || tokens.is_empty() {
        return Vec::new();
    }
    let mut by_length: Vec<&MentionToken> = tokens.iter().filter(|t| !t.text.is_empty()).collect();
    by_length.sort_by_key(|t| std::cmp::Reverse(t.text.len()));
    let mut seen = std::collections::HashSet::new();
    by_length.retain(|t| seen.insert(t.text.as_str()));

    let mut taken: Vec<MentionRange> = Vec::new();
    for token in by_length {
        let mut from = 0;
        while let Some(rel) = text[from..].find(&token.text) {
            let start = from + rel;
            let end = start + token.text.len();
            from = start + 1;
            if !stands_alone(text, start, end) {
                continue;
            }
            if taken
                .iter()
                .any(|range| start < range.end && end > range.start)
            {
                continue;
            }
            taken.push(MentionRange {
                start,
                end,
                kind: token.kind,
            });
        }
    }
    taken.sort_by_key(|r| r.start);
    taken
}

/// Backspace at the end of a mention removes the whole token (App rule).
pub fn delete_mention_atomically(
    prev: &str,
    next: &str,
    ranges: &[MentionRange],
    caret: usize,
) -> Option<(String, usize)> {
    if ranges.is_empty() || caret == 0 || !prev.is_char_boundary(caret) {
        return None;
    }
    let range = *ranges.iter().find(|r| r.end == caret)?;
    let removed = prev[..caret].chars().next_back()?;
    let cut = caret - removed.len_utf8();
    let mut one_removed = String::with_capacity(prev.len() - removed.len_utf8());
    one_removed.push_str(&prev[..cut]);
    one_removed.push_str(&prev[caret..]);
    if one_removed != next {
        return None;
    }
    let mut text = String::with_capacity(prev.len() - (range.end - range.start));
    text.push_str(&prev[..range.start]);
    text.push_str(&prev[range.end..]);
    Some((text, range.start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_find_computers_and_files() {
        let tokens = [
            MentionToken {
                text: "@mini".into(),
                kind: MentionKind::Computer,
            },
            MentionToken {
                text: "src/main.rs".into(),
                kind: MentionKind::File,
            },
        ];
        let ranges = mention_ranges("see @mini and src/main.rs please", &tokens);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].kind, MentionKind::Computer);
        assert_eq!(ranges[1].kind, MentionKind::File);
    }

    #[test]
    fn atomic_backspace_eats_whole_mention() {
        let prev = "hi @mini";
        let caret = 8;
        let next = "hi @min";
        let ranges = mention_ranges(
            prev,
            &[MentionToken {
                text: "@mini".into(),
                kind: MentionKind::Computer,
            }],
        );
        assert_eq!(
            ranges,
            [MentionRange {
                start: 3,
                end: 8,
                kind: MentionKind::Computer
            }]
        );
        let (text, at) = delete_mention_atomically(prev, next, &ranges, caret).unwrap();
        assert_eq!(text, "hi ");
        assert_eq!(at, 3);
    }

    #[test]
    fn email_is_not_a_mention() {
        let ranges = mention_ranges(
            "a@b.com",
            &[MentionToken {
                text: "@b".into(),
                kind: MentionKind::Computer,
            }],
        );
        assert!(ranges.is_empty());
    }
}
