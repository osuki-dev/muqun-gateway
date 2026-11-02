//! Code highlighting: syntect behind Blinc's [`SyntaxHighlighter`] trait.
//!
//! Blinc's trait is regex-rule shaped (`token_rules`), but its
//! `highlight(&self, text) -> StyledText` is overridable, so syntect's
//! stateful, grammar-driven parse replaces the rules wholesale: the same
//! highlighter serves markdown fences here and can be handed to
//! `code().syntax(SyntaxConfig::new(..))` for Diff / Shell output later.
//!
//! Grammars: syntect's bundled Sublime set (`default-syntaxes`, ~75
//! languages), parsed with the pure-Rust `fancy-regex` engine. Colours: a
//! syntect `Theme` built at run time from the app theme's `ColorToken`s, so
//! code follows the light/dark scheme like everything else. Results are
//! cached by (language, code, palette).

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
use syntect::parsing::SyntaxSet;

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

/// Token colours, from the app theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodePalette {
    pub text: Color,
    pub comment: Color,
    pub keyword: Color,
    pub string: Color,
    pub number: Color,
    pub function: Color,
    pub punctuation: Color,
    pub error: Color,
    pub background: Color,
}

impl CodePalette {
    pub fn current() -> Self {
        let t = ThemeState::get();
        Self {
            text: t.color(ColorToken::TextPrimary),
            comment: t.color(ColorToken::TextTertiary),
            keyword: t.color(ColorToken::Primary),
            string: t.color(ColorToken::Success),
            number: t.color(ColorToken::Warning),
            function: t.color(ColorToken::Info),
            punctuation: t.color(ColorToken::TextSecondary),
            error: t.color(ColorToken::Error),
            background: t.color(ColorToken::SurfaceElevated),
        }
    }

    fn key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for c in [
            self.text,
            self.comment,
            self.keyword,
            self.string,
            self.number,
            self.function,
            self.punctuation,
            self.error,
        ] {
            for v in [c.r, c.g, c.b, c.a] {
                v.to_bits().hash(&mut h);
            }
        }
        h.finish()
    }

    /// A syntect theme whose scopes map onto these colours.
    fn theme(&self) -> Theme {
        let syn = |c: Color| SynColor {
            r: (c.r.clamp(0.0, 1.0) * 255.0) as u8,
            g: (c.g.clamp(0.0, 1.0) * 255.0) as u8,
            b: (c.b.clamp(0.0, 1.0) * 255.0) as u8,
            a: 255,
        };
        let item = |scopes: &str, color: Color, style: Option<FontStyle>| ThemeItem {
            scope: ScopeSelectors::from_str(scopes).expect("static scope selector"),
            style: StyleModifier {
                foreground: Some(syn(color)),
                background: None,
                font_style: style,
            },
        };
        Theme {
            name: Some("muqun".into()),
            author: None,
            settings: ThemeSettings {
                foreground: Some(syn(self.text)),
                ..ThemeSettings::default()
            },
            scopes: vec![
                item("comment, punctuation.definition.comment", self.comment, Some(FontStyle::ITALIC)),
                item("string, string punctuation.definition.string, constant.character.escape", self.string, None),
                item("constant.numeric, constant.language, constant.other, support.constant", self.number, None),
                item(
                    "keyword, storage, storage.type, storage.modifier, keyword.operator.word",
                    self.keyword,
                    None,
                ),
                item(
                    "entity.name.function, support.function, meta.function-call variable.function, entity.name.type, support.type, entity.name.class, entity.name.tag",
                    self.function,
                    None,
                ),
                item("entity.other.attribute-name, variable.parameter", self.number, None),
                item("punctuation, keyword.operator", self.punctuation, None),
                item("invalid, markup.deleted", self.error, None),
                item("markup.inserted", self.string, None),
                item("markup.heading, markup.bold", self.keyword, Some(FontStyle::BOLD)),
            ],
        }
    }
}

/// A syntect-backed [`SyntaxHighlighter`] for one language.
pub struct SyntectHighlighter {
    lang: Option<String>,
    palette: CodePalette,
    theme: Theme,
}

impl SyntectHighlighter {
    pub fn new(lang: Option<&str>, palette: CodePalette) -> Self {
        Self {
            lang: lang.map(str::to_string),
            theme: palette.theme(),
            palette,
        }
    }

    fn plain(&self, text: &str) -> StyledText {
        StyledText::from_lines(
            text.split('\n')
                .map(|line| {
                    StyledLine::new(
                        line,
                        vec![TextSpan::colored(0, line.len(), self.palette.text)],
                    )
                })
                .collect(),
        )
    }
}

/// The grammar for a fence name. The bundled set has no TypeScript, TOML
/// or shell-session grammar; their nearest relatives stand in.
fn find<'s>(set: &'s SyntaxSet, lang: &str) -> Option<&'s syntect::parsing::SyntaxReference> {
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

fn to_color(c: SynColor) -> Color {
    Color::rgba(
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        c.a as f32 / 255.0,
    )
}

impl SyntaxHighlighter for SyntectHighlighter {
    fn token_rules(&self) -> &[TokenRule] {
        &[]
    }

    fn default_color(&self) -> Color {
        self.palette.text
    }

    fn background_color(&self) -> Color {
        self.palette.background
    }

    fn line_number_color(&self) -> Color {
        self.palette.comment
    }

    fn highlight(&self, text: &str) -> StyledText {
        let set = syntaxes();
        let syntax = self.lang.as_deref().and_then(|l| find(set, l));
        let Some(syntax) = syntax.filter(|_| text.len() <= MAX_HIGHLIGHT_BYTES) else {
            return self.plain(text);
        };
        let mut h = HighlightLines::new(syntax, &self.theme);
        let mut lines = Vec::new();
        for line in text.split('\n') {
            // The grammars are the "newlines" variant: give each line its
            // newline, then leave it out of the spans.
            let with_nl = format!("{line}\n");
            let Ok(regions) = h.highlight_line(&with_nl, set) else {
                return self.plain(text);
            };
            let mut spans = Vec::with_capacity(regions.len());
            let mut at = 0;
            for (style, piece) in regions {
                let piece = piece.strip_suffix('\n').unwrap_or(piece);
                let end = (at + piece.len()).min(line.len());
                if end > at {
                    let mut span = TextSpan::colored(at, end, to_color(style.foreground));
                    span.bold = style.font_style.contains(FontStyle::BOLD);
                    span.italic = style.font_style.contains(FontStyle::ITALIC);
                    spans.push(span);
                }
                at = end;
            }
            lines.push(StyledLine::new(line, spans));
        }
        StyledText::from_lines(lines)
    }
}

/// `code` highlighted as `lang` in the current theme, from the cache when
/// it has been seen.
pub fn highlight(lang: Option<&str>, code: &str) -> Arc<StyledText> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Arc<StyledText>>>> = OnceLock::new();
    let palette = CodePalette::current();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    lang.hash(&mut h);
    code.hash(&mut h);
    palette.key().hash(&mut h);
    let key = h.finish();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(&key) {
        return hit.clone();
    }
    let styled = Arc::new(SyntectHighlighter::new(lang, palette).highlight(code));
    let mut c = cache.lock().unwrap();
    if c.len() >= CACHE_ENTRIES {
        c.clear();
    }
    c.insert(key, styled.clone());
    styled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> CodePalette {
        let c = |r| Color::rgba(r, 0.0, 0.0, 1.0);
        CodePalette {
            text: c(0.1),
            comment: c(0.2),
            keyword: c(0.3),
            string: c(0.4),
            number: c(0.5),
            function: c(0.6),
            punctuation: c(0.7),
            error: c(0.8),
            background: c(0.9),
        }
    }

    fn close(a: Color, b: Color) -> bool {
        (a.r - b.r).abs() < 0.01 && (a.g - b.g).abs() < 0.01 && (a.b - b.b).abs() < 0.01
    }

    #[test]
    fn rust_tokens_take_the_palette_colours_and_spans_cover_each_line() {
        let p = palette();
        let code = "// note\nfn main() {\n    let s = \"hi\"; let n = 42;\n}";
        let styled = SyntectHighlighter::new(Some("rust"), p).highlight(code);
        assert_eq!(styled.lines.len(), 4);
        let color_of = |line: usize, needle: &str| {
            let l = &styled.lines[line];
            let at = l.text.find(needle).unwrap();
            l.spans
                .iter()
                .find(|s| s.start <= at && at < s.end)
                .map(|s| s.color)
                .unwrap()
        };
        assert!(close(color_of(0, "note"), p.comment));
        assert!(close(color_of(1, "fn"), p.keyword));
        assert!(close(color_of(2, "hi"), p.string));
        assert!(close(color_of(2, "42"), p.number));
        for line in &styled.lines {
            let covered: usize = line.spans.iter().map(|s| s.end - s.start).sum();
            assert_eq!(covered, line.text.len(), "{:?}", line.text);
        }
    }

    #[test]
    fn unknown_languages_and_huge_code_are_plain() {
        let p = palette();
        let styled = SyntectHighlighter::new(Some("no-such-lang"), p).highlight("a\nb");
        assert_eq!(styled.lines.len(), 2);
        assert!(close(styled.lines[0].spans[0].color, p.text));
        let huge = "x".repeat(MAX_HIGHLIGHT_BYTES + 1);
        let styled = SyntectHighlighter::new(Some("rust"), p).highlight(&huge);
        assert!(close(styled.lines[0].spans[0].color, p.text));
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
