//! The terminal key vocabulary: which key names `send-keys` accepts, and how a
//! chord such as `ctrl+shift+enter` is taken apart before an adapter spells it
//! for its own backend.
//!
//! The names are the app's own (`src/lib/terminal-keys.ts`): lower case, `esc`
//! rather than `escape`, `pageup`, `f1`..`f12`, modifiers joined with `+` in
//! the order `ctrl+alt+shift+<base>`. Discovery advertises the subset each
//! backend can deliver as a [`KeyboardVocabulary`], and a chord outside it is
//! refused with [`BackendError::KeyUnsupported`] -- a `400 key_unsupported` at
//! the HTTP edge -- before anything reaches the pane.

use serde::{Deserialize, Serialize};

use super::BackendError;

/// Bumped when the shape of [`KeyboardVocabulary`] changes.
pub const KEYBOARD_VERSION: u32 = 1;

/// What one backend can deliver, as discovery advertises it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardVocabulary {
    pub version: u32,
    /// Named keys this backend delivers on their own.
    pub bases: Vec<String>,
    /// Modifiers that may prefix a base, in their fixed order.
    pub modifiers: Vec<String>,
    /// Whether a modifier + special-key chord (anything outside the classic
    /// set, see [`KeyChord::is_classic`]) reaches the pane.
    pub extended: bool,
}

impl KeyboardVocabulary {
    pub fn new(bases: &[NamedKey], extended: bool) -> Self {
        Self {
            version: KEYBOARD_VERSION,
            bases: bases.iter().map(|key| key.name().into_owned()).collect(),
            modifiers: MODIFIERS.iter().map(|m| (*m).to_owned()).collect(),
            extended,
        }
    }
}

const MODIFIERS: [&str; 3] = ["ctrl", "alt", "shift"];

/// A named, non-printing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Esc,
    Tab,
    Backspace,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

impl NamedKey {
    /// Every named key, in the order discovery lists them.
    pub const ALL: [NamedKey; 27] = [
        Self::Enter,
        Self::Esc,
        Self::Tab,
        Self::Backspace,
        Self::Space,
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
        Self::Home,
        Self::End,
        Self::PageUp,
        Self::PageDown,
        Self::Insert,
        Self::Delete,
        Self::F(1),
        Self::F(2),
        Self::F(3),
        Self::F(4),
        Self::F(5),
        Self::F(6),
        Self::F(7),
        Self::F(8),
        Self::F(9),
        Self::F(10),
        Self::F(11),
        Self::F(12),
    ];

    /// Accepts the vocabulary spelling plus the aliases `send-keys` has always
    /// taken (`escape`, `arrowup`, ...). `lower` must already be lower case.
    fn parse(lower: &str) -> Option<Self> {
        Some(match lower {
            "enter" => Self::Enter,
            "esc" | "escape" => Self::Esc,
            "tab" => Self::Tab,
            "backspace" => Self::Backspace,
            "space" => Self::Space,
            "up" | "arrowup" => Self::Up,
            "down" | "arrowdown" => Self::Down,
            "left" | "arrowleft" => Self::Left,
            "right" | "arrowright" => Self::Right,
            "home" => Self::Home,
            "end" => Self::End,
            "pageup" => Self::PageUp,
            "pagedown" => Self::PageDown,
            "insert" => Self::Insert,
            "delete" => Self::Delete,
            _ => {
                let number: u8 = lower.strip_prefix('f')?.parse().ok()?;
                if !(1..=12).contains(&number) || lower.starts_with("f0") {
                    return None;
                }
                Self::F(number)
            }
        })
    }

    /// The vocabulary spelling.
    pub fn name(self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(match self {
            Self::Enter => "enter",
            Self::Esc => "esc",
            Self::Tab => "tab",
            Self::Backspace => "backspace",
            Self::Space => "space",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Home => "home",
            Self::End => "end",
            Self::PageUp => "pageup",
            Self::PageDown => "pagedown",
            Self::Insert => "insert",
            Self::Delete => "delete",
            Self::F(number) => return std::borrow::Cow::Owned(format!("f{number}")),
        })
    }
}

/// What a chord is pressed on: a named key or one printable character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyBase {
    Named(NamedKey),
    Char(char),
}

/// One key press, modifiers included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub base: KeyBase,
}

impl KeyChord {
    /// Parses one `send-keys` entry, or `None` when it names no key.
    ///
    /// A single printable character is that character, even `+`. Otherwise
    /// modifiers are peeled off the front -- in any order, each at most once --
    /// and what remains must be a named key or one printable character, so
    /// `ctrl++` is Control and plus. Case is ignored except in a printable base
    /// under `alt`/`shift`, where `alt+X` and `alt+x` are different keys.
    pub fn parse(value: &str) -> Option<Self> {
        if let Some(c) = single_printable(value) {
            return Some(Self::plain(KeyBase::Char(c)));
        }
        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        let mut rest = value;
        loop {
            let lower = rest.to_ascii_lowercase();
            let (flag, len) = if lower.starts_with("ctrl+") {
                (&mut ctrl, 5)
            } else if lower.starts_with("alt+") {
                (&mut alt, 4)
            } else if lower.starts_with("shift+") {
                (&mut shift, 6)
            } else {
                break;
            };
            if *flag || rest.len() == len {
                return None;
            }
            *flag = true;
            rest = &rest[len..];
        }
        let base = match single_printable(rest) {
            // Control folds case: `ctrl+C` has always meant `ctrl+c`.
            Some(' ') => KeyBase::Named(NamedKey::Space),
            Some(c) if ctrl => KeyBase::Char(c.to_ascii_lowercase()),
            Some(c) => KeyBase::Char(c),
            None => KeyBase::Named(NamedKey::parse(&rest.to_ascii_lowercase())?),
        };
        Some(Self {
            ctrl,
            alt,
            shift,
            base,
        })
    }

    fn plain(base: KeyBase) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            base,
        }
    }

    pub fn has_modifiers(&self) -> bool {
        self.ctrl || self.alt || self.shift
    }

    /// Whether this chord works without the pane's extended-key protocol: a
    /// plain key, `ctrl+<a-z>`, `ctrl+[`, `ctrl+]`, `ctrl+\`, `ctrl+space`, or
    /// `shift+tab`. Every one of them has a single legacy byte sequence.
    pub fn is_classic(&self) -> bool {
        match (self.ctrl, self.alt, self.shift, self.base) {
            (false, false, false, _) => true,
            (true, false, false, KeyBase::Char(c)) => {
                c.is_ascii_lowercase() || matches!(c, '[' | ']' | '\\')
            }
            (true, false, false, KeyBase::Named(NamedKey::Space)) => true,
            (false, false, true, KeyBase::Named(NamedKey::Tab)) => true,
            _ => false,
        }
    }

    /// The vocabulary spelling, `ctrl+alt+shift+<base>`.
    pub fn name(&self) -> String {
        let mut name = String::new();
        for (on, modifier) in [
            (self.ctrl, "ctrl+"),
            (self.alt, "alt+"),
            (self.shift, "shift+"),
        ] {
            if on {
                name.push_str(modifier);
            }
        }
        match self.base {
            KeyBase::Named(key) => name.push_str(&key.name()),
            KeyBase::Char(c) => name.push(c),
        }
        name
    }

    /// The bytes a vt220/xterm keyboard sends for a key of the editing block
    /// (`home`, `end`, `pageup`, `pagedown`, `insert`, `delete`), modifiers
    /// included; `None` for any other key. For a backend whose own key names
    /// stop short of this block (herdr), which then types the bytes instead.
    ///
    /// Plain keys use the vt220 `CSI n ~` spellings, which readline, nvim,
    /// less and the agent TUIs read whatever the cursor-key mode. A modifier
    /// becomes xterm's parameter `m` = 1 + shift 1 + alt 2 + ctrl 4: `CSI 1;m H`
    /// and `CSI 1;m F` for home and end, `CSI n;m ~` for the rest.
    pub fn editing_sequence(&self) -> Option<String> {
        let KeyBase::Named(key) = self.base else {
            return None;
        };
        let (code, final_letter) = match key {
            NamedKey::Home => (1, Some('H')),
            NamedKey::End => (4, Some('F')),
            NamedKey::Insert => (2, None),
            NamedKey::Delete => (3, None),
            NamedKey::PageUp => (5, None),
            NamedKey::PageDown => (6, None),
            _ => return None,
        };
        let modifier = 1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl);
        Some(match (modifier, final_letter) {
            (1, _) => format!("\x1b[{code}~"),
            (modifier, Some(letter)) => format!("\x1b[1;{modifier}{letter}"),
            (modifier, None) => format!("\x1b[{code};{modifier}~"),
        })
    }
}

fn single_printable(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let c = chars.next()?;
    (chars.next().is_none() && !c.is_control()).then_some(c)
}

/// The refusal for a name that is no key at all.
pub fn unknown_key(value: &str) -> BackendError {
    BackendError::KeyUnsupported(format!("unsupported key {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(value: &str) -> KeyChord {
        KeyChord::parse(value).unwrap_or_else(|| panic!("{value:?} should parse"))
    }

    #[test]
    fn every_advertised_base_parses_back_to_itself() {
        for key in NamedKey::ALL {
            let name = key.name();
            assert_eq!(chord(&name).base, KeyBase::Named(key), "{name}");
            assert_eq!(chord(&name).name(), name);
        }
    }

    #[test]
    fn modifiers_parse_in_any_order_but_print_in_the_fixed_one() {
        assert_eq!(chord("shift+ctrl+enter").name(), "ctrl+shift+enter");
        assert_eq!(chord("Shift+Alt+Ctrl+Home").name(), "ctrl+alt+shift+home");
        assert_eq!(chord("ctrl++").base, KeyBase::Char('+'));
        assert_eq!(chord("+").base, KeyBase::Char('+'));
        assert_eq!(chord("ctrl+C").name(), "ctrl+c");
        assert_eq!(chord("alt+X").name(), "alt+X");
        assert_eq!(chord("ctrl+ ").name(), "ctrl+space");
    }

    #[test]
    fn nonsense_is_not_a_key() {
        for value in [
            "",
            "ctrl+",
            "ctrl+ctrl+c",
            "hyper+a",
            "f0",
            "f13",
            "f01",
            "-t",
            "C-x;kill-server",
            "ctrl+enterx",
            "\n",
        ] {
            assert!(KeyChord::parse(value).is_none(), "{value:?}");
        }
    }

    #[test]
    fn the_classic_set_is_exactly_what_has_a_legacy_byte_sequence() {
        for value in [
            "enter",
            "f5",
            "home",
            "a",
            ";",
            "ctrl+a",
            "ctrl+z",
            "ctrl+[",
            "ctrl+]",
            "ctrl+\\",
            "ctrl+space",
            "shift+tab",
        ] {
            assert!(chord(value).is_classic(), "{value}");
        }
        for value in [
            "ctrl+enter",
            "shift+enter",
            "alt+enter",
            "alt+x",
            "ctrl+1",
            "ctrl+up",
            "shift+f5",
            "ctrl+shift+home",
            "ctrl+alt+a",
            "ctrl+shift+tab",
            "ctrl+tab",
        ] {
            assert!(!chord(value).is_classic(), "{value}");
        }
    }

    #[test]
    fn the_editing_block_has_its_vt220_and_xterm_bytes() {
        for (value, bytes) in [
            ("home", "\x1b[1~"),
            ("end", "\x1b[4~"),
            ("pageup", "\x1b[5~"),
            ("pagedown", "\x1b[6~"),
            ("insert", "\x1b[2~"),
            ("delete", "\x1b[3~"),
            ("shift+home", "\x1b[1;2H"),
            ("alt+end", "\x1b[1;3F"),
            ("ctrl+home", "\x1b[1;5H"),
            ("ctrl+shift+end", "\x1b[1;6F"),
            ("ctrl+alt+shift+home", "\x1b[1;8H"),
            ("shift+pageup", "\x1b[5;2~"),
            ("ctrl+pagedown", "\x1b[6;5~"),
            ("alt+insert", "\x1b[2;3~"),
            ("ctrl+delete", "\x1b[3;5~"),
        ] {
            assert_eq!(
                chord(value).editing_sequence().as_deref(),
                Some(bytes),
                "{value}"
            );
        }
        for value in ["enter", "up", "f5", "a", "ctrl+a", "ctrl+up", "space"] {
            assert_eq!(chord(value).editing_sequence(), None, "{value}");
        }
    }

    #[test]
    fn vocabulary_serialises_in_the_contract_shape() {
        let vocabulary = KeyboardVocabulary::new(&NamedKey::ALL, true);
        let value = serde_json::to_value(&vocabulary).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "version": 1,
                "bases": ["enter","esc","tab","backspace","space","up","down","left","right",
                          "home","end","pageup","pagedown","insert","delete",
                          "f1","f2","f3","f4","f5","f6","f7","f8","f9","f10","f11","f12"],
                "modifiers": ["ctrl","alt","shift"],
                "extended": true
            })
        );
    }
}
