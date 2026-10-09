/// Quote one argv value for the interactive shell running in a pane.
/// Values are data: they are always single-quoted and never interpreted as
/// shell structure assembled by the caller.
///
/// Follow-up: fish is not quite POSIX inside single quotes -- it treats `\'`
/// and `\\` as escapes there -- so a value containing a backslash followed by
/// a quote or a second backslash reaches a fish pane altered. Paths and flags
/// in practice do not carry these; a fish-aware quoting is not done yet.
pub(crate) fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
