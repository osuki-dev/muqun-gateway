//! Keys while the terminal has the keyboard: which ones it keeps, which go to
//! the app, and how a Rune key becomes the event libghostty encodes.
//!
//! The shortcut plan (Linux and Windows; ⌘ chords stay the app's on macOS):
//!
//! | Keys | Goes to |
//! |---|---|
//! | Ctrl+` | toggles the panel (closing it hands the keyboard back) |
//! | Ctrl+Shift+C / Ctrl+Shift+V | copy the selection / paste (bracketed) |
//! | Ctrl+Insert / Shift+Insert | the same, as X11 and dictation tools send them |
//! | any other Ctrl+Shift+key | the app: its own Ctrl+Shift shortcut, else its Ctrl+key one (Ctrl+Shift+K opens the palette) |
//! | everything else, Esc, Tab and Ctrl+letters included | the shell |

use libghostty_vt::key::Key as GKey;
use rune::{Key, KeyEvent, PhysicalKeyCode};

/// Where a key typed into the terminal goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Terminal,
    /// Hand to the app's window shortcuts.
    App,
    Copy,
    Paste,
    Toggle,
}

/// Whether `ev` is Ctrl+` (the panel's own toggle, on every platform).
pub fn is_toggle(ev: &KeyEvent, physical: Option<PhysicalKeyCode>) -> bool {
    let m = ev.mods;
    let backquote = physical == Some(PhysicalKeyCode::Backquote)
        || matches!(&ev.key, Key::Command('`'))
        || matches!(&ev.key, Key::Text(t) if t == "`");
    backquote && m.ctrl && !m.alt && !m.shift
}

/// Route a key the terminal heard. `mac`: ⌘ chords are the app's, as in
/// Terminal.app and every macOS terminal.
pub fn route(ev: &KeyEvent, physical: Option<PhysicalKeyCode>, mac: bool) -> Route {
    if is_toggle(ev, physical) {
        return Route::Toggle;
    }
    let m = ev.mods;
    // The old CUA pair, which clipboard tools (dictation pasting through
    // Shift+Insert) still send, on every platform.
    if ev.key == Key::Insert && !m.alt {
        match (m.ctrl, m.shift) {
            (true, false) => return Route::Copy,
            (false, true) => return Route::Paste,
            _ => {}
        }
    }
    let letter = |c: char| {
        matches!(&ev.key, Key::Command(k) if k.eq_ignore_ascii_case(&c))
            || physical == Some(letter_code(c))
    };
    if mac {
        if m.logo {
            return match () {
                _ if letter('c') && !m.shift => Route::Copy,
                _ if letter('v') && !m.shift => Route::Paste,
                _ => Route::App,
            };
        }
        return Route::Terminal;
    }
    if m.ctrl && m.shift && !m.alt {
        return match () {
            _ if letter('c') => Route::Copy,
            _ if letter('v') => Route::Paste,
            _ => Route::App,
        };
    }
    Route::Terminal
}

fn letter_code(c: char) -> PhysicalKeyCode {
    match c {
        'c' => PhysicalKeyCode::KeyC,
        _ => PhysicalKeyCode::KeyV,
    }
}

/// The modifiers libghostty's encoder reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

/// A key press for the emulator thread: libghostty's key, the text it
/// types (if any) and the character it types unshifted (Kitty protocol).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyInput {
    pub key: GKey,
    pub mods: Mods,
    pub text: Option<String>,
    pub unshifted: char,
    pub repeat: bool,
}

/// What Rune delivered, as libghostty wants it. `None` for keys a terminal
/// doesn't send (a lone modifier).
pub fn key_input(
    ev: &KeyEvent,
    physical: Option<PhysicalKeyCode>,
    base: Option<char>,
    repeat: bool,
) -> Option<KeyInput> {
    let m = ev.mods;
    let mods = Mods {
        shift: m.shift,
        ctrl: m.ctrl,
        alt: m.alt,
        logo: m.logo,
    };
    let (logical, text, ch) = match &ev.key {
        Key::Text(t) => {
            let c = t.chars().next()?;
            (from_char(c), Some(t.clone()), Some(c))
        }
        Key::Command(c) => (from_char(*c), None, Some(*c)),
        Key::Enter => (GKey::Enter, None, None),
        Key::Backspace => (GKey::Backspace, None, None),
        Key::Delete => (GKey::Delete, None, None),
        Key::Left => (GKey::ArrowLeft, None, None),
        Key::Right => (GKey::ArrowRight, None, None),
        Key::Up => (GKey::ArrowUp, None, None),
        Key::Down => (GKey::ArrowDown, None, None),
        Key::Home => (GKey::Home, None, None),
        Key::End => (GKey::End, None, None),
        Key::PageUp => (GKey::PageUp, None, None),
        Key::PageDown => (GKey::PageDown, None, None),
        Key::Insert => (GKey::Insert, None, None),
        Key::Escape => (GKey::Escape, None, None),
        Key::Tab => (GKey::Tab, None, None),
        Key::Function(n) => (function(*n)?, None, None),
        _ => return None,
    };
    // The physical key names it whatever the layout types; text keys and
    // keypad keys follow it, the logical key covers scripts and synthesized
    // input (no physical origin).
    let key = physical
        .and_then(from_physical)
        .filter(|k| *k != GKey::Unidentified)
        .unwrap_or(logical);
    // Control characters never travel as text: the encoder derives them
    // from the key and Ctrl.
    let text = text.filter(|t| !t.chars().any(char::is_control) && !(m.ctrl && !m.alt));
    let unshifted = base
        .or_else(|| ch.map(|c| c.to_ascii_lowercase()))
        .unwrap_or('\0');
    Some(KeyInput {
        key,
        mods,
        text,
        unshifted,
        repeat,
    })
}

fn function(n: u8) -> Option<GKey> {
    use GKey::*;
    const F: [GKey; 24] = [
        F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20,
        F21, F22, F23, F24,
    ];
    F.get(usize::from(n).checked_sub(1)?).copied()
}

/// The US key that types `c`, for input with no physical key behind it.
fn from_char(c: char) -> GKey {
    use GKey::*;
    const LETTERS: [GKey; 26] = [
        A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    ];
    const DIGITS: [GKey; 10] = [
        Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    ];
    match c.to_ascii_lowercase() {
        l @ 'a'..='z' => LETTERS[(l as u8 - b'a') as usize],
        d @ '0'..='9' => DIGITS[(d as u8 - b'0') as usize],
        ' ' => Space,
        '`' | '~' => Backquote,
        '-' | '_' => Minus,
        '=' | '+' => Equal,
        '[' | '{' => BracketLeft,
        ']' | '}' => BracketRight,
        '\\' | '|' => Backslash,
        ';' | ':' => Semicolon,
        '\'' | '"' => Quote,
        ',' | '<' => Comma,
        '.' | '>' => Period,
        '/' | '?' => Slash,
        _ => Unidentified,
    }
}

fn from_physical(code: PhysicalKeyCode) -> Option<GKey> {
    use GKey as G;
    use PhysicalKeyCode as P;
    Some(match code {
        P::Backquote => G::Backquote,
        P::Backslash => G::Backslash,
        P::BracketLeft => G::BracketLeft,
        P::BracketRight => G::BracketRight,
        P::Comma => G::Comma,
        P::Digit0 => G::Digit0,
        P::Digit1 => G::Digit1,
        P::Digit2 => G::Digit2,
        P::Digit3 => G::Digit3,
        P::Digit4 => G::Digit4,
        P::Digit5 => G::Digit5,
        P::Digit6 => G::Digit6,
        P::Digit7 => G::Digit7,
        P::Digit8 => G::Digit8,
        P::Digit9 => G::Digit9,
        P::Equal => G::Equal,
        P::IntlBackslash => G::IntlBackslash,
        P::IntlRo => G::IntlRo,
        P::IntlYen => G::IntlYen,
        P::KeyA => G::A,
        P::KeyB => G::B,
        P::KeyC => G::C,
        P::KeyD => G::D,
        P::KeyE => G::E,
        P::KeyF => G::F,
        P::KeyG => G::G,
        P::KeyH => G::H,
        P::KeyI => G::I,
        P::KeyJ => G::J,
        P::KeyK => G::K,
        P::KeyL => G::L,
        P::KeyM => G::M,
        P::KeyN => G::N,
        P::KeyO => G::O,
        P::KeyP => G::P,
        P::KeyQ => G::Q,
        P::KeyR => G::R,
        P::KeyS => G::S,
        P::KeyT => G::T,
        P::KeyU => G::U,
        P::KeyV => G::V,
        P::KeyW => G::W,
        P::KeyX => G::X,
        P::KeyY => G::Y,
        P::KeyZ => G::Z,
        P::Minus => G::Minus,
        P::Period => G::Period,
        P::Quote => G::Quote,
        P::Semicolon => G::Semicolon,
        P::Slash => G::Slash,
        P::Backspace => G::Backspace,
        P::Enter => G::Enter,
        P::Space => G::Space,
        P::Tab => G::Tab,
        P::Delete => G::Delete,
        P::End => G::End,
        P::Home => G::Home,
        P::Insert => G::Insert,
        P::PageDown => G::PageDown,
        P::PageUp => G::PageUp,
        P::ArrowDown => G::ArrowDown,
        P::ArrowLeft => G::ArrowLeft,
        P::ArrowRight => G::ArrowRight,
        P::ArrowUp => G::ArrowUp,
        P::Escape => G::Escape,
        P::Numpad0 => G::Numpad0,
        P::Numpad1 => G::Numpad1,
        P::Numpad2 => G::Numpad2,
        P::Numpad3 => G::Numpad3,
        P::Numpad4 => G::Numpad4,
        P::Numpad5 => G::Numpad5,
        P::Numpad6 => G::Numpad6,
        P::Numpad7 => G::Numpad7,
        P::Numpad8 => G::Numpad8,
        P::Numpad9 => G::Numpad9,
        P::NumpadAdd => G::NumpadAdd,
        P::NumpadDecimal => G::NumpadDecimal,
        P::NumpadDivide => G::NumpadDivide,
        P::NumpadEnter => G::NumpadEnter,
        P::NumpadMultiply => G::NumpadMultiply,
        P::NumpadSubtract => G::NumpadSubtract,
        P::F1 => G::F1,
        P::F2 => G::F2,
        P::F3 => G::F3,
        P::F4 => G::F4,
        P::F5 => G::F5,
        P::F6 => G::F6,
        P::F7 => G::F7,
        P::F8 => G::F8,
        P::F9 => G::F9,
        P::F10 => G::F10,
        P::F11 => G::F11,
        P::F12 => G::F12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Mods as RMods;

    fn ev(key: Key, ctrl: bool, shift: bool, logo: bool) -> KeyEvent {
        KeyEvent {
            key,
            mods: RMods {
                shift,
                cmd: ctrl,
                alt: false,
                ctrl,
                logo,
            },
        }
    }

    #[test]
    fn terminal_keeps_shell_keys_and_lends_ctrl_shift_to_the_app() {
        use PhysicalKeyCode as P;
        let cases = [
            // Shell keys, Esc and Ctrl+letters included.
            (ev(Key::Escape, false, false, false), None, Route::Terminal),
            (ev(Key::Tab, false, false, false), None, Route::Terminal),
            (
                ev(Key::Command('c'), true, false, false),
                Some(P::KeyC),
                Route::Terminal,
            ),
            (
                ev(Key::Command('k'), true, false, false),
                Some(P::KeyK),
                Route::Terminal,
            ),
            (
                ev(Key::Command('w'), true, false, false),
                Some(P::KeyW),
                Route::Terminal,
            ),
            // Copy, paste, the toggle, and the rest of Ctrl+Shift for the app.
            (
                ev(Key::Command('c'), true, true, false),
                Some(P::KeyC),
                Route::Copy,
            ),
            (
                ev(Key::Command('v'), true, true, false),
                Some(P::KeyV),
                Route::Paste,
            ),
            (
                ev(Key::Command('`'), true, false, false),
                Some(P::Backquote),
                Route::Toggle,
            ),
            (
                ev(Key::Command('k'), true, true, false),
                Some(P::KeyK),
                Route::App,
            ),
            (ev(Key::Insert, false, true, false), None, Route::Paste),
            (ev(Key::Insert, true, false, false), None, Route::Copy),
            (ev(Key::Insert, false, false, false), None, Route::Terminal),
            (
                ev(Key::Command('t'), true, true, false),
                Some(P::KeyT),
                Route::App,
            ),
        ];
        for (i, (e, physical, want)) in cases.into_iter().enumerate() {
            assert_eq!(route(&e, physical, false), want, "case {i}: {e:?}");
        }
        // macOS: ⌘ chords are the app's, Ctrl chords the shell's.
        let cmd_c = ev(Key::Command('c'), false, false, true);
        assert_eq!(route(&cmd_c, Some(P::KeyC), true), Route::Copy);
        let cmd_k = ev(Key::Command('k'), false, false, true);
        assert_eq!(route(&cmd_k, Some(P::KeyK), true), Route::App);
        let ctrl_c = ev(Key::Command('c'), true, false, false);
        assert_eq!(route(&ctrl_c, Some(P::KeyC), true), Route::Terminal);
    }

    #[test]
    fn ctrl_letters_travel_as_keys_and_text_as_text() {
        let ctrl_c = key_input(
            &ev(Key::Command('c'), true, false, false),
            Some(PhysicalKeyCode::KeyC),
            Some('c'),
            false,
        )
        .unwrap();
        assert_eq!(
            (ctrl_c.key, ctrl_c.text, ctrl_c.unshifted),
            (GKey::C, None, 'c')
        );
        // A shifted symbol keeps its text; the physical key is the digit.
        let bang = key_input(
            &ev(Key::Text("!".into()), false, true, false),
            Some(PhysicalKeyCode::Digit1),
            Some('1'),
            false,
        )
        .unwrap();
        assert_eq!((bang.key, bang.text.as_deref()), (GKey::Digit1, Some("!")));
        // No physical key (a script): the logical key stands in.
        let esc = key_input(&ev(Key::Escape, false, false, false), None, None, false).unwrap();
        assert_eq!(esc.key, GKey::Escape);
    }
}
