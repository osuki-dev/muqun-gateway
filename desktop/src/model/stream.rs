//! The streaming item's text, split for cheap updates (architecture §2).
//!
//! While an assistant part streams, its row shows two things: the *settled*
//! prefix -- every markdown block that a later block has already followed,
//! rendered as markdown once per block -- and the *tail*, the block still
//! being written, shown as plain text through `Text::bound` so a delta only
//! swaps text in place. [`settled_boundary`] finds the split;
//! [`Throttle`] keeps the tail's updates at or under 30 Hz.
//!
//! Splitting at a blank line outside a fenced code block gives the same
//! blocks the whole text will parse into, with two cosmetic exceptions while
//! streaming (a loose list restarts per segment; a reference link defined
//! later resolves late). The finished part is parsed again as a whole.

use std::time::{Duration, Instant};

/// At most this often the tail is replaced: 30 Hz.
pub const STREAM_INTERVAL: Duration = Duration::from_millis(33);

/// A fence opener or closer: up to three spaces, then at least three
/// backticks or tildes. Returns the fence character and its length.
fn fence(line: &str) -> Option<(char, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = rest.chars().take_while(|c| *c == ch).count();
    (run >= 3).then_some((ch, run))
}

/// The byte offset where the settled prefix ends: just past the last blank
/// line, or the last closing fence, that is not inside a fenced code block.
/// `0` while the first block is still being written.
pub fn settled_boundary(text: &str) -> usize {
    let mut boundary = 0;
    let mut open: Option<(char, usize)> = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let end = at + line.len();
        let complete = line.ends_with('\n');
        let content = line.trim_end_matches(['\n', '\r']);
        match open {
            Some((ch, len)) => {
                // A closer is the same character, at least as long, and
                // nothing after it.
                if let Some((c, l)) = fence(content) {
                    if c == ch && l >= len && content.trim().chars().all(|x| x == ch) {
                        open = None;
                        if complete {
                            boundary = end;
                        }
                    }
                }
            }
            None => {
                if let Some(f) = fence(content) {
                    open = Some(f);
                } else if complete && content.trim().is_empty() {
                    boundary = end;
                }
            }
        }
        at = end;
    }
    boundary
}

/// How the tail is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailKind {
    Prose,
    /// The tail is an open code fence; shown in the monospace face without
    /// its opening line.
    Code,
}

/// The tail after `boundary`, as shown.
pub fn tail(text: &str, boundary: usize) -> (TailKind, &str) {
    let rest = text.get(boundary..).unwrap_or("");
    let rest = rest.trim_start_matches(['\n', '\r']);
    let first = rest.split('\n').next().unwrap_or("");
    if fence(first).is_some() {
        let body = rest.split_once('\n').map(|(_, b)| b).unwrap_or("");
        (TailKind::Code, body)
    } else {
        (TailKind::Prose, rest.trim_end())
    }
}

/// Lets an update through at most once per `interval`; one that arrives
/// sooner is remembered, and [`Throttle::take_due`] releases it once the
/// interval has passed.
#[derive(Debug, Clone)]
pub struct Throttle {
    interval: Duration,
    last: Option<Instant>,
    pending: bool,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            pending: false,
        }
    }

    /// An update arrived at `now`. True when it may be shown now (and is
    /// counted as shown); false when it waits for [`Self::take_due`].
    pub fn offer(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < self.interval)
        {
            self.pending = true;
            return false;
        }
        self.last = Some(now);
        self.pending = false;
        true
    }

    /// True once when a held-back update may now be shown.
    pub fn take_due(&mut self, now: Instant) -> bool {
        if !self.pending {
            return false;
        }
        self.offer(now)
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new(STREAM_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boundary_follows_blank_lines() {
        assert_eq!(settled_boundary("Hello wor"), 0);
        assert_eq!(settled_boundary("One.\n\nTwo"), 6);
        assert_eq!(settled_boundary("One.\n\nTwo.\n\n"), 12);
        assert_eq!(
            settled_boundary("One.\n  \nTwo"),
            8,
            "whitespace-only is blank"
        );
        assert_eq!(
            settled_boundary("One.\n"),
            0,
            "a single newline is not a block end"
        );
    }

    #[test]
    fn blank_lines_inside_a_fence_do_not_settle() {
        let open = "Intro\n\n```rust\nfn a() {}\n\nfn b() {}\n";
        assert_eq!(settled_boundary(open), 7, "only the intro is settled");
        let closed = format!("{open}```\nAfter");
        assert_eq!(
            settled_boundary(&closed),
            open.len() + 4,
            "a closing fence settles the block"
        );
        let tildes = "~~~\n```\n\n~~~\nx";
        assert_eq!(
            settled_boundary(tildes),
            13,
            "a backtick run does not close tildes"
        );
    }

    #[test]
    fn the_tail_of_an_open_fence_is_code_without_its_opener() {
        let text = "Intro\n\n```rust\nfn main() {\n";
        let b = settled_boundary(text);
        assert_eq!(tail(text, b), (TailKind::Code, "fn main() {\n"));
        assert_eq!(tail("Hi\n\nthere ", 4), (TailKind::Prose, "there"));
    }

    #[test]
    fn the_boundary_never_splits_a_character() {
        let text = "第一段。\n\n第二段";
        let b = settled_boundary(text);
        assert!(text.is_char_boundary(b));
        assert_eq!(tail(text, b).1, "第二段");
    }

    #[test]
    fn the_throttle_lets_one_update_per_interval_through_and_keeps_the_last() {
        let start = Instant::now();
        let ms = |n| start + Duration::from_millis(n);
        let mut t = Throttle::new(Duration::from_millis(33));
        assert!(t.offer(ms(0)));
        assert!(!t.offer(ms(5)));
        assert!(!t.offer(ms(20)));
        assert!(t.is_pending());
        assert!(!t.take_due(ms(30)), "not yet");
        assert!(t.take_due(ms(34)), "released once due");
        assert!(!t.take_due(ms(80)), "nothing left to release");
        assert!(t.offer(ms(80)));
    }

    #[test]
    fn a_burst_at_frame_rate_is_shown_at_most_thirty_times_a_second() {
        let start = Instant::now();
        let mut t = Throttle::default();
        let mut shown = 0;
        // 120 Hz deltas for one second, the drain running every frame.
        for frame in 0..120u64 {
            let now = start + Duration::from_micros(frame * 8_333);
            if t.offer(now) || t.take_due(now) {
                shown += 1;
            }
        }
        assert!((25..=31).contains(&shown), "shown {shown} times");
    }
}
