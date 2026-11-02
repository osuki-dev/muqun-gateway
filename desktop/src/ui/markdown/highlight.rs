//! Code highlighting: syntect behind Blinc's [`SyntaxHighlighter`] trait.
//!
//! Blinc's trait is regex-rule shaped (`token_rules`), but its
//! `highlight(&self, text) -> StyledText` is overridable, so syntect's
//! stateful, grammar-driven parse replaces the rules wholesale: the same
//! highlighter serves markdown fences here and can be handed to
//! `code().syntax(SyntaxConfig::new(..))` for Diff / Shell output later.
//!
//! Grammars: syntect's bundled Sublime set (`default-syntaxes`, ~75
//! languages), parsed with the pure-Rust `fancy-regex` engine.
//!
//! Colours never come from a syntect theme. syntect is given a theme whose
//! scope rules carry a *token role* ([`Role`]) instead of a colour, so a
//! parse yields roles; each role is a theme `ColorToken`, resolved against
//! the current `ThemeState` every time the code is rendered. A theme or
//! scheme switch therefore recolours code without parsing it again, and no
//! colour exists here that the theme did not supply. The role parse is
//! cached by (language, code).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};

use blinc_core::Color;
use blinc_layout::styled_text::{StyledLine, StyledText, TextSpan};
use blinc_layout::syntax::{SyntaxHighlighter, TokenRule};
use blinc_theme::{ColorToken, ThemeState};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Code longer than this is shown plain: highlighting is linear but not
/// free, and nobody reads a 200 KB fence token by token.
const MAX_HIGHLIGHT_BYTES: usize = 120 * 1024;
const CACHE_ENTRIES: usize = 256;

/// The grammar set, loaded once (tens of milliseconds); [`warm`] does it
/// off the UI thread at startup.
fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Load the grammars in the background so the first fence does not pay.
pub fn warm() {
    std::thread::Builder::new()
        .name("muqun-syntax".into())
        .spawn(|| {
            let started = std::time::Instant::now();
            let n = syntaxes().syntaxes().len();
            tracing::debug!(
                grammars = n,
                ms = started.elapsed().as_millis() as u64,
                "syntaxes loaded"
            );
        })
        .ok();
}

/// What a token is, as far as colour goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Role {
    Text = 0,
    Comment,
    Keyword,
    String,
    Number,
    Function,
    Punctuation,
    Error,
}

const ROLES: [Role; 8] = [
    Role::Text,
    Role::Comment,
    Role::Keyword,
    Role::String,
    Role::Number,
    Role::Function,
    Role::Punctuation,
    Role::Error,
];

impl Role {
    /// The theme token a role is drawn in.
    pub fn token(self) -> ColorToken {
        match self {
            Role::Text => ColorToken::TextPrimary,
            Role::Comment => ColorToken::TextTertiary,
            Role::Keyword => ColorToken::Primary,
            Role::String => ColorToken::Success,
            Role::Number => ColorToken::Warning,
            Role::Function => ColorToken::Info,
            Role::Punctuation => ColorToken::TextSecondary,
            Role::Error => ColorToken::Error,
        }
    }

    /// The role's marker in syntect's colour channel.
    fn marker(self) -> SynColor {
        SynColor {
            r: self as u8,
            g: 0,
            b: 0,
            a: 255,
        }
    }

    fn from_marker(c: SynColor) -> Role {
        ROLES.get(c.r as usize).copied().unwrap_or(Role::Text)
    }
}

/// Role colours, read from the theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodePalette {
    colors: [Color; 8],
}

impl CodePalette {
    /// The current theme's colours.
    pub fn current() -> Self {
        let t = ThemeState::get();
        Self {
            colors: ROLES.map(|r| t.color(r.token())),
        }
    }

    pub fn color(&self, role: Role) -> Color {
        self.colors[role as usize]
    }
}

/// The syntect theme mapping scopes to roles (built once; it holds no
/// colours).
fn role_theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let item = |scopes: &str, role: Role, style: Option<FontStyle>| ThemeItem {
            scope: ScopeSelectors::from_str(scopes).expect("static scope selector"),
            style: StyleModifier {
                foreground: Some(role.marker()),
                background: None,
                font_style: style,
            },
        };
        Theme {
            name: Some("muqun-roles".into()),
            author: None,
            settings: ThemeSettings {
                foreground: Some(Role::Text.marker()),
                ..ThemeSettings::default()
            },
            scopes: vec![
                item("comment, punctuation.definition.comment", Role::Comment, Some(FontStyle::ITALIC)),
                item(
                    "string, string punctuation.definition.string, constant.character.escape",
                    Role::String,
                    None,
                ),
                item(
                    "constant.numeric, constant.language, constant.other, support.constant",
                    Role::Number,
                    None,
                ),
                item(
                    "keyword, storage, storage.type, storage.modifier, keyword.operator.word",
                    Role::Keyword,
                    None,
                ),
                item(
                    "entity.name.function, support.function, meta.function-call variable.function, entity.name.type, support.type, entity.name.class, entity.name.tag",
                    Role::Function,
                    None,
                ),
                item("entity.other.attribute-name, variable.parameter", Role::Number, None),
                item("punctuation, keyword.operator", Role::Punctuation, None),
                item("invalid, markup.deleted", Role::Error, None),
                item("markup.inserted", Role::String, None),
                item("markup.heading, markup.bold", Role::Keyword, Some(FontStyle::BOLD)),
            ],
        }
    })
}

/// A run of one role within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub start: usize,
    pub end: usize,
    pub role: Role,
    pub bold: bool,
    pub italic: bool,
}

/// Highlighted code as roles, line by line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roles {
    pub lines: Vec<(String, Vec<Run>)>,
}

impl Roles {
    fn plain(text: &str) -> Self {
        Self {
            lines: text
                .split('\n')
                .map(|l| {
                    let runs = vec![Run {
                        start: 0,
                        end: l.len(),
                        role: Role::Text,
                        bold: false,
                        italic: false,
                    }];
                    (l.to_string(), runs)
                })
                .collect(),
        }
    }

    /// Colour the roles with `palette`.
    pub fn styled(&self, palette: &CodePalette) -> StyledText {
        StyledText::from_lines(
            self.lines
                .iter()
                .map(|(text, runs)| {
                    let spans = runs
                        .iter()
                        .map(|r| {
                            let mut span = TextSpan::colored(r.start, r.end, palette.color(r.role));
                            span.bold = r.bold;
                            span.italic = r.italic;
                            span
                        })
                        .collect();
                    StyledLine::new(text.clone(), spans)
                })
                .collect(),
        )
    }
}

/// The grammar for a fence name. The bundled set has no TypeScript, TOML
/// or shell-session grammar; their nearest relatives stand in.
fn find<'s>(set: &'s SyntaxSet, lang: &str) -> Option<&'s SyntaxReference> {
    let lang = lang.to_ascii_lowercase();
    let alias = match lang.as_str() {
        "ts" | "typescript" | "tsx" | "jsx" | "mjs" | "cjs" => "js",
        "toml" | "ini" | "conf" => "properties",
        "shell" | "zsh" | "console" | "shellsession" | "fish" => "sh",
        "jsonc" | "json5" => "json",
        other => other,
    };
    set.find_syntax_by_token(alias)
        .or_else(|| set.find_syntax_by_name(alias))
        .or_else(|| set.find_syntax_by_extension(alias))
}

/// Parse `text` as `lang` into roles.
pub fn roles(lang: Option<&str>, text: &str) -> Roles {
    let set = syntaxes();
    let Some(syntax) = lang
        .and_then(|l| find(set, l))
        .filter(|_| text.len() <= MAX_HIGHLIGHT_BYTES)
    else {
        return Roles::plain(text);
    };
    let mut h = HighlightLines::new(syntax, role_theme());
    let mut lines = Vec::new();
    for line in text.split('\n') {
        // The grammars are the "newlines" variant: give each line its
        // newline, then leave it out of the runs.
        let with_nl = format!("{line}\n");
        let Ok(regions) = h.highlight_line(&with_nl, set) else {
            return Roles::plain(text);
        };
        let mut runs = Vec::with_capacity(regions.len());
        let mut at = 0;
        for (style, piece) in regions {
            let piece = piece.strip_suffix('\n').unwrap_or(piece);
            let end = (at + piece.len()).min(line.len());
            if end > at {
                runs.push(Run {
                    start: at,
                    end,
                    role: Role::from_marker(style.foreground),
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                });
            }
            at = end;
        }
        lines.push((line.to_string(), runs));
    }
    Roles { lines }
}

/// A syntect-backed [`SyntaxHighlighter`] for one language, coloured from
/// the theme current when it highlights.
pub struct SyntectHighlighter {
    lang: Option<String>,
}

impl SyntectHighlighter {
    pub fn new(lang: Option<&str>) -> Self {
        Self {
            lang: lang.map(str::to_string),
        }
    }
}

impl SyntaxHighlighter for SyntectHighlighter {
    fn token_rules(&self) -> &[TokenRule] {
        &[]
    }

    fn default_color(&self) -> Color {
        ThemeState::get().color(ColorToken::TextPrimary)
    }

    fn background_color(&self) -> Color {
        ThemeState::get().color(ColorToken::SurfaceElevated)
    }

    fn line_number_color(&self) -> Color {
        ThemeState::get().color(ColorToken::TextTertiary)
    }

    fn highlight(&self, text: &str) -> StyledText {
        cached_roles(self.lang.as_deref(), text).styled(&CodePalette::current())
    }
}

fn cached_roles(lang: Option<&str>, code: &str) -> Arc<Roles> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Arc<Roles>>>> = OnceLock::new();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    lang.hash(&mut h);
    code.hash(&mut h);
    let key = h.finish();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(&key) {
        return hit.clone();
    }
    let parsed = Arc::new(roles(lang, code));
    let mut c = cache.lock().unwrap();
    if c.len() >= CACHE_ENTRIES {
        c.clear();
    }
    c.insert(key, parsed.clone());
    parsed
}

/// `code` highlighted as `lang` in the current theme's colours.
pub fn highlight(lang: Option<&str>, code: &str) -> StyledText {
    SyntectHighlighter::new(lang).highlight(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role_at(r: &Roles, line: usize, needle: &str) -> Role {
        let (text, runs) = &r.lines[line];
        let at = text.find(needle).unwrap();
        runs.iter()
            .find(|run| run.start <= at && at < run.end)
            .map(|run| run.role)
            .unwrap()
    }

    #[test]
    fn rust_tokens_get_their_roles_and_runs_cover_each_line() {
        let code = "// note\nfn main() {\n    let s = \"hi\"; let n = 42;\n}";
        let r = roles(Some("rust"), code);
        assert_eq!(r.lines.len(), 4);
        assert_eq!(role_at(&r, 0, "note"), Role::Comment);
        assert_eq!(role_at(&r, 1, "fn"), Role::Keyword);
        assert_eq!(role_at(&r, 2, "hi"), Role::String);
        assert_eq!(role_at(&r, 2, "42"), Role::Number);
        for (text, runs) in &r.lines {
            let covered: usize = runs.iter().map(|run| run.end - run.start).sum();
            assert_eq!(covered, text.len(), "{text:?}");
        }
    }

    #[test]
    fn roles_are_coloured_by_the_palette_given() {
        let r = roles(Some("rust"), "fn x() {}");
        let a = CodePalette {
            // Distinct per role; test-only values, never drawn.
            colors: ROLES.map(|role| Color {
                r: role as u8 as f32 / 10.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            }),
        };
        let styled = r.styled(&a);
        let fn_span = &styled.lines[0].spans[0];
        assert_eq!(fn_span.color, a.color(Role::Keyword));
    }

    #[test]
    fn unknown_languages_and_huge_code_are_plain() {
        let r = roles(Some("no-such-lang"), "a\nb");
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0].1[0].role, Role::Text);
        let huge = "x".repeat(MAX_HIGHLIGHT_BYTES + 1);
        assert_eq!(roles(Some("rust"), &huge).lines[0].1[0].role, Role::Text);
    }

    #[test]
    fn common_fence_names_resolve() {
        let set = syntaxes();
        let mut missing = Vec::new();
        for lang in [
            "rust", "rs", "python", "py", "js", "ts", "tsx", "json", "sh", "bash", "zsh", "yaml",
            "go", "c", "cpp", "java", "diff", "html", "css", "sql", "md", "toml",
        ] {
            if find(set, lang).is_none() {
                missing.push(lang);
            }
        }
        assert!(missing.is_empty(), "no grammar for {missing:?}");
    }
}
