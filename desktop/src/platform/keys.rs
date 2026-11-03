//! Keyboard shortcuts as the person reads them (architecture §10.1).
//!
//! The UI names a shortcut logically -- `Primary+K`, `Primary+Shift+T` --
//! and this table writes it the way the platform does: symbols run together
//! on macOS (`⌘K`, `⌘⇧T`), words joined by `+` elsewhere (`Ctrl+K`,
//! `Ctrl+Shift+T`). `Primary` is ⌘ on macOS and Ctrl everywhere else.

/// The shortcuts the window answers to (`ui/mod.rs` `on_key`). The whole
/// table is the contract; a view names its entry when it shows the hint.
pub const SEARCH: &str = "Primary+K";
#[allow(dead_code)]
pub const NEW_SESSION: &str = "Primary+N";
pub const SETTINGS: &str = "Primary+,";
#[allow(dead_code)]
pub const STOP: &str = "Primary+.";
#[allow(dead_code)]
pub const HOME: &str = "Primary+0";
#[allow(dead_code)]
pub const THEMES: &str = "Primary+Shift+T";

/// Which convention to write a shortcut in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// macOS: `⌘⇧K`.
    Symbols,
    /// Windows and Linux: `Ctrl+Shift+K`.
    Words,
}

impl Style {
    /// This platform's convention.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Symbols
        } else {
            Self::Words
        }
    }
}

/// `logical` (`Primary+Shift+K`) as this platform writes it.
pub fn label(logical: &str) -> String {
    label_in(logical, Style::current())
}

/// `logical` as `style` writes it. Modifiers are `Primary`, `Shift`, `Alt`
/// and `Ctrl` (the real Control key, ⌃ on macOS), in any order; macOS lists
/// them in its own order (⌃⌥⇧⌘). The last part is the key, kept as written.
pub fn label_in(logical: &str, style: Style) -> String {
    let mut parts: Vec<&str> = logical.split('+').collect();
    // `Primary++` names the plus key itself.
    let key = if logical.ends_with("++") {
        parts.truncate(parts.len().saturating_sub(2));
        "+"
    } else {
        parts.pop().unwrap_or_default()
    };
    let has = |m: &str| parts.iter().any(|p| p.eq_ignore_ascii_case(m));
    match style {
        Style::Symbols => {
            let mut out = String::new();
            for (name, symbol) in [
                ("Ctrl", "⌃"),
                ("Alt", "⌥"),
                ("Shift", "⇧"),
                ("Primary", "⌘"),
            ] {
                if has(name) {
                    out.push_str(symbol);
                }
            }
            out.push_str(key);
            out
        }
        Style::Words => {
            let mut out: Vec<&str> = Vec::new();
            if has("Primary") || has("Ctrl") {
                out.push("Ctrl");
            }
            if has("Alt") {
                out.push("Alt");
            }
            if has("Shift") {
                out.push("Shift");
            }
            out.push(key);
            out.join("+")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_writes_symbols_and_everyone_else_words() {
        assert_eq!(label_in(SEARCH, Style::Symbols), "⌘K");
        assert_eq!(label_in(SEARCH, Style::Words), "Ctrl+K");
        assert_eq!(label_in(STOP, Style::Symbols), "⌘.");
        assert_eq!(label_in(STOP, Style::Words), "Ctrl+.");
        assert_eq!(label_in(SETTINGS, Style::Words), "Ctrl+,");
        assert_eq!(label_in(NEW_SESSION, Style::Symbols), "⌘N");
        assert_eq!(label_in(HOME, Style::Words), "Ctrl+0");
    }

    #[test]
    fn modifiers_follow_each_platforms_order() {
        assert_eq!(label_in(THEMES, Style::Symbols), "⇧⌘T");
        assert_eq!(label_in("Shift+Primary+T", Style::Words), "Ctrl+Shift+T");
        assert_eq!(label_in("Primary+Alt+Shift+P", Style::Symbols), "⌥⇧⌘P");
        assert_eq!(label_in("Primary++", Style::Words), "Ctrl++");
        assert_eq!(label_in("Primary++", Style::Symbols), "⌘+");
    }

    #[test]
    fn this_platform_has_one_style() {
        let expected = if cfg!(target_os = "macos") {
            "⌘K"
        } else {
            "Ctrl+K"
        };
        assert_eq!(label(SEARCH), expected);
    }
}
