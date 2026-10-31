/// Quote one argv value for the interactive shell running in a pane.
/// Values are data: they are always single-quoted and never interpreted as
/// shell structure assembled by the caller.
///
/// The same word reads back as `value` in sh, bash, zsh and fish. fish is not
/// POSIX inside single quotes -- it reads `\'` and `\\` there as escapes --
/// so neither a quote nor a backslash is ever left inside the quotes: each
/// one closes them and goes outside as `\'` or `\\`, which every one of
/// those shells reads as that one character.
pub(crate) fn shell_word(value: &str) -> String {
    let mut word = String::with_capacity(value.len() + 2);
    word.push('\'');
    for c in value.chars() {
        match c {
            '\'' => word.push_str("'\\''"),
            '\\' => word.push_str("'\\\\'"),
            c => word.push(c),
        }
    }
    word.push('\'');
    word
}

#[cfg(test)]
mod tests {
    use super::shell_word;

    /// A word as POSIX shells read it: literal inside `'…'`, `\x` is `x`
    /// outside.
    fn posix_read(word: &str) -> String {
        read(word, |_| None)
    }

    /// A word as fish reads it: like POSIX, except that inside `'…'` a
    /// backslash escapes a quote or a second backslash.
    fn fish_read(word: &str) -> String {
        read(word, |next| matches!(next, '\'' | '\\').then_some(next))
    }

    fn read(word: &str, quoted_escape: impl Fn(char) -> Option<char>) -> String {
        let (mut out, mut quoted, mut chars) = (String::new(), false, word.chars().peekable());
        while let Some(c) = chars.next() {
            match (quoted, c) {
                (_, '\'') => quoted = !quoted,
                (true, '\\') => match chars.peek().copied().and_then(&quoted_escape) {
                    Some(escaped) => {
                        chars.next();
                        out.push(escaped);
                    }
                    None => out.push('\\'),
                },
                (false, '\\') => out.extend(chars.next()),
                (_, c) => out.push(c),
            }
        }
        out
    }

    #[test]
    #[cfg_attr(windows, ignore = "not yet ported to Windows; see docs/windows.md")]
    fn a_word_reads_back_unchanged_in_every_pane_shell() {
        let shells = ["bash", "zsh", "fish"];
        for value in [
            "plain",
            "two words",
            "a'b;$(touch nope)",
            // The fish breakout: `\'` must not close or reopen the quotes.
            r"x\'; touch /tmp/pwned; echo \'",
            r"trailing \",
            r"a\\b",
            "$HOME `id` $(id)",
            "line one\nline two",
        ] {
            let word = shell_word(value);
            assert_eq!(posix_read(&word), value, "sh/bash/zsh: {word}");
            assert_eq!(fish_read(&word), value, "fish: {word}");
            // And through each shell this machine has.
            for shell in shells {
                let Ok(out) = std::process::Command::new(shell)
                    .arg("-c")
                    .arg(format!("printf %s {word}"))
                    .output()
                else {
                    continue;
                };
                assert_eq!(
                    String::from_utf8_lossy(&out.stdout),
                    value,
                    "{shell}: {word}"
                );
            }
        }
    }
}
