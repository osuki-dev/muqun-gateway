//! The exit marker `exec.run` makes the line it types into a pane print, and
//! the readers that find it again or hide it from clients.
//!
//! The typed line is [`command_line`]: the command runs in an inner `sh`, and
//! an outer `sh` then prints one line `\x1fmuqun-exit:<task id>:<code>\x1f`.
//! tmux drops C0 controls it does not interpret, so on tmux the `\x1f`
//! delimiters are gone and the line is the bare `muqun-exit:<task id>:<code>`:
//! readers accept both.

use std::borrow::Cow;

use serde_json::Value;

use super::shell_word::shell_word;

pub(crate) const MARKER: char = '\u{1f}';
const TAG: &str = "muqun-exit:";
/// The format string as the pane echoes the typed line. It never matches a
/// marker, so its last occurrence is where the command line was typed.
const ECHOED_FORMAT: &str = "muqun-exit:%s";

/// The line typed into the pane: run `command`, then print the marker for
/// `job_id` with its exit status.
///
/// Shell-neutral on purpose. The interactive shell only parses argv words and
/// single-quoted literals -- nothing like `$?` that fish spells differently --
/// and an `exit N` in `command` ends the inner `sh` only, so the status is
/// still printed. `job_id` is a task id: URL-safe, never quoted.
pub(crate) fn command_line(command: &str, job_id: &str) -> String {
    format!(
        "sh -c 'sh -c \"$1\"; printf \"\\037{TAG}%s:%d\\037\\n\" \"$2\" \"$?\"' sh {} {job_id}",
        shell_word(command)
    )
}

/// The job id and exit code a marker line carries, if `line` is one.
fn parse_marker(line: &str) -> Option<(&str, i32)> {
    let line = line.trim();
    let line = line.strip_prefix(MARKER).unwrap_or(line);
    let line = line.strip_suffix(MARKER).unwrap_or(line);
    let (id, code) = line.strip_prefix(TAG)?.split_once(':')?;
    let digits = code.strip_prefix('-').unwrap_or(code);
    let valid_id = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid_id || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((id, code.parse().ok()?))
}

/// The exit code of the first marker line for `job_id` in `text`. The
/// primitive; `run_command` reads through [`find_exit_after_command`].
#[cfg(test)]
pub(crate) fn find_exit(text: &str, job_id: &str) -> Option<i32> {
    text.lines()
        .filter_map(parse_marker)
        .find_map(|(id, code)| (id == job_id).then_some(code))
}

/// Where this run's own output can be in `text`, and whether the echoed
/// command line bounds it.
///
/// With the echo in view, only the lines after it count: a marker printed
/// before it is not this run's. Once a long output has scrolled the echo out
/// of the window, the whole window counts -- the tab was opened for this run
/// and `job_id` is a random nonce, so a marker carrying it cannot have come
/// before the typed line. `None` while the echo is still being typed.
fn run_window(text: &str) -> Option<(&str, bool)> {
    match text.rfind(ECHOED_FORMAT) {
        Some(echoed) => text[echoed..]
            .split_once('\n')
            .map(|(_, after)| (after, true)),
        None => Some((text, false)),
    }
}

/// The marker line for `job_id` in `window`: the first after the echo, or the
/// last in a window the echo has scrolled out of. Its byte offset and code.
fn marker_in(window: &str, job_id: &str, after_echo: bool) -> Option<(usize, i32)> {
    let mut offset = 0;
    let mut found = None;
    for line in window.split_inclusive('\n') {
        if let Some((_, code)) = parse_marker(line).filter(|(id, _)| *id == job_id) {
            found = Some((offset, code));
            if after_echo {
                break;
            }
        }
        offset += line.len();
    }
    found
}

/// This run's exit code, once its marker shows. See [`run_window`].
pub(crate) fn find_exit_after_command(text: &str, job_id: &str) -> Option<i32> {
    let (window, after_echo) = run_window(text)?;
    marker_in(window, job_id, after_echo).map(|(_, code)| code)
}

/// What the command printed: the window of [`run_window`] up to this run's
/// marker line (or to the end while it is still running).
pub(crate) fn output_after_command<'a>(text: &'a str, job_id: &str) -> &'a str {
    let Some((window, after_echo)) = run_window(text) else {
        return "";
    };
    match marker_in(window, job_id, after_echo) {
        Some((end, _)) => &window[..end],
        None => window,
    }
}

/// `text` without its marker lines: what a client is shown of a pane.
pub(crate) fn strip_exit_markers(text: &str) -> Cow<'_, str> {
    if !text.lines().any(|line| parse_marker(line).is_some()) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.split_inclusive('\n')
            .filter(|line| parse_marker(line).is_none())
            .collect(),
    )
}

/// Drop the marker rows of a captured-history page, and count what is left.
/// Recording already leaves them out; this catches a checkpoint written
/// before it did.
pub(crate) fn strip_history_page(page: &mut Value) {
    let Some(rows) = page.get_mut("rows").and_then(Value::as_array_mut) else {
        return;
    };
    rows.retain(|row| row.as_str().is_none_or(|row| parse_marker(row).is_none()));
    let count = rows.len();
    if page.get("row_count").is_some() {
        page["row_count"] = count.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_matching_job_marker_counts() {
        let text = "$ sh -c 'x'; printf '\u{1f}muqun-exit:%s:%d\u{1f}\\n' t_abc $?\nhi\n\u{1f}muqun-exit:t_other:9\u{1f}\n\u{1f}muqun-exit:t_abc:3\u{1f}\n$ ";
        assert_eq!(find_exit(text, "t_abc"), Some(3));
        assert_eq!(find_exit(text, "t_zzz"), None);
    }

    #[test]
    fn a_forged_marker_printed_by_the_command_is_ignored_when_it_lacks_the_job_id() {
        let text = "\u{1f}muqun-exit:t_abc:0\u{1f}\n$ sh -c 'printf …'; printf '\u{1f}muqun-exit:%s:%d\u{1f}\\n' t_abc $?\n\u{1f}muqun-exit:t_abc:5\u{1f}\n";
        assert_eq!(
            find_exit_after_command(text, "t_abc"),
            Some(5),
            "the marker before the command line is not ours"
        );
    }

    #[test]
    fn stripping_removes_marker_lines_only() {
        let text = "a\n\u{1f}muqun-exit:t_1:0\u{1f}\nb\n";
        assert_eq!(strip_exit_markers(text), "a\nb\n");
        assert!(matches!(strip_exit_markers("plain"), Cow::Borrowed(_)));
        // A history page counts the rows it serves.
        let mut page = serde_json::json!({"rows": ["a", "muqun-exit:t_1:0", "b"], "row_count": 3});
        strip_history_page(&mut page);
        assert_eq!(
            page,
            serde_json::json!({"rows": ["a", "b"], "row_count": 2})
        );
    }

    #[test]
    fn a_marker_tmux_printed_without_its_delimiters_still_counts_and_is_stripped() {
        // What `capture-pane` returns: tmux drops the unit separators.
        let echo = format!("$ {}", command_line("exit 3", "t_abc"));
        let text = format!("{echo}\nmuqun-exit:t_abc:3\n$ ");
        assert_eq!(find_exit_after_command(&text, "t_abc"), Some(3));
        assert_eq!(strip_exit_markers(&text), format!("{echo}\n$ "));
    }

    #[test]
    fn nothing_counts_while_the_typed_line_is_still_being_echoed() {
        let partial = "$ sh -c 'sh -c \"$1\"; printf \"\\037muqun-exit:%s:%d";
        assert_eq!(find_exit_after_command(partial, "t_abc"), None);
        assert_eq!(output_after_command(partial, "t_abc"), "");
        assert_eq!(find_exit_after_command("$ ", "t_abc"), None);
    }

    #[test]
    fn once_the_echo_has_scrolled_away_the_last_marker_with_the_id_counts() {
        // A long output pushed the typed line out of the read window.
        let text =
            "line 1\nline 2\nmuqun-exit:t_other:9\nline 3\n\u{1f}muqun-exit:t_abc:4\u{1f}\n$ ";
        assert_eq!(find_exit_after_command(text, "t_abc"), Some(4));
        assert_eq!(
            output_after_command(text, "t_abc"),
            "line 1\nline 2\nmuqun-exit:t_other:9\nline 3\n"
        );
        assert_eq!(
            output_after_command("line 1\nline 2", "t_abc"),
            "line 1\nline 2"
        );
    }

    #[test]
    fn the_typed_line_runs_the_command_and_prints_its_status() {
        let line = command_line("echo 'hi there'; exit 3", "t_abc");
        assert_eq!(
            line,
            "sh -c 'sh -c \"$1\"; printf \"\\037muqun-exit:%s:%d\\037\\n\" \"$2\" \"$?\"' sh 'echo '\\''hi there'\\''; exit 3' t_abc"
        );
        assert!(line.contains(ECHOED_FORMAT));
        // What the interactive shell does with it: `exit 3` ends the inner
        // `sh` only, and the status comes through.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&line)
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(text, "hi there\n\u{1f}muqun-exit:t_abc:3\u{1f}\n");
        assert_eq!(find_exit(&text, "t_abc"), Some(3));
    }
}
