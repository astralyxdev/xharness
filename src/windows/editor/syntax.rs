//! Syntax highlighting for the editor, with syntect and bat's grammars
//! (two-face): Rust, TypeScript/TSX, Python, Go, Swift, C/C++, JSON, YAML,
//! TOML, Markdown, shell, and more.
//!
//! Grammars name every bit of a line with TextMate scopes; a small theme
//! sorts the scopes into a few [`Token`]s, which [`color`] paints from the
//! app's palette. Highlighting stays the same when the app switches between
//! light and dark; only the colors change.
//!
//! Lines are highlighted in order, each starting from the [`State`] the
//! line before it ended in, so an editor can re-highlight from an edited
//! line and stop once the state matches what it had before.

use std::{path::Path, str::FromStr, sync::OnceLock};

use gpui::{FontStyle, FontWeight, Hsla};
use syntect::{
    highlighting::{
        Color, HighlightState, Highlighter, RangedHighlightIterator, ScopeSelectors, StyleModifier, Theme,
        ThemeItem, ThemeSettings,
    },
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
};

/// What a stretch of code is, as far as coloring goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Token {
    Plain,
    Comment,
    String,
    /// Numbers, constants, escapes.
    Number,
    Keyword,
    Type,
    Function,
    /// Object keys and fields.
    Property,
    Tag,
    Attribute,
    /// Operators and punctuation.
    Punctuation,
    Heading,
    Emphasis,
    Strong,
    Inserted,
    Deleted,
}

impl Token {
    const ALL: [Token; 16] = [
        Token::Plain,
        Token::Comment,
        Token::String,
        Token::Number,
        Token::Keyword,
        Token::Type,
        Token::Function,
        Token::Property,
        Token::Tag,
        Token::Attribute,
        Token::Punctuation,
        Token::Heading,
        Token::Emphasis,
        Token::Strong,
        Token::Inserted,
        Token::Deleted,
    ];
}

/// Scope selectors for each token, most general first; syntect picks the
/// most specific match, so `storage.type.function` (Rust's `fn`) beats
/// `storage.type` (a type) and comes out a keyword.
const RULES: &[(&str, Token)] = &[
    ("comment, punctuation.definition.comment", Token::Comment),
    ("string, markup.raw, markup.inline.raw, punctuation.definition.string", Token::String),
    (
        "constant, support.constant, variable.other.constant, entity.name.constant, \
         constant.character.escape, constant.other.placeholder",
        Token::Number,
    ),
    (
        "keyword, storage, storage.modifier, variable.language, keyword.operator.word, \
         keyword.operator.logical.python, keyword.operator.new, keyword.operator.expression, \
         storage.type.function, storage.type.class, storage.type.struct, storage.type.enum, \
         storage.type.impl, storage.type.trait, storage.type.module, storage.type.type, \
         storage.type.interface, storage.type.namespace, storage.type.var, storage.type.let, \
         storage.type.const, storage.type.keyword, punctuation.section.interpolation, punctuation.definition.template-expression",
        Token::Keyword,
    ),
    (
        "storage.type, entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, \
         entity.name.trait, entity.name.interface, entity.name.impl, entity.name.namespace, \
         entity.name.module, support.type, support.class, support.module, entity.other.inherited-class",
        Token::Type,
    ),
    (
        "entity.name.function, support.function, variable.function, entity.name.method, \
         support.macro, entity.name.macro, meta.function-call.identifier, markup.underline.link",
        Token::Function,
    ),
    (
        "variable.other.member, variable.other.property, variable.other.object.property, \
         support.variable.property, support.type.property-name, meta.object-literal.key, \
         entity.name.tag.yaml, meta.mapping.key string, entity.name.key, \
         keyword.other.definition.ini, entity.name.label",
        Token::Property,
    ),
    ("entity.name.tag, punctuation.definition.tag", Token::Tag),
    (
        "entity.other.attribute-name, meta.annotation, meta.attribute, storage.type.annotation, \
         punctuation.definition.annotation, entity.name.function.decorator, \
         storage.modifier.lifetime, entity.name.lifetime, punctuation.definition.lifetime",
        Token::Attribute,
    ),
    ("keyword.operator, punctuation, meta.brace", Token::Punctuation),
    ("markup.heading, entity.name.section, punctuation.definition.heading", Token::Heading),
    ("markup.italic", Token::Emphasis),
    ("markup.bold", Token::Strong),
    ("markup.inserted", Token::Inserted),
    ("markup.deleted, invalid", Token::Deleted),
];

/// Words some grammars scope as types (`storage.type`) that read as
/// keywords: Rust's and TypeScript's `let` and `const`, Go's `var`.
const DECLARATIONS: &[&str] = &[
    "let", "const", "var", "static", "fn", "func", "function", "def", "class", "struct", "enum", "union",
    "impl", "trait", "type", "interface", "namespace", "module", "mod", "extern", "crate", "typedef",
    "macro_rules",
];

/// A colored stretch of a line, by byte offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub token: Token,
}

/// Where highlighting is at the start of a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    parse: ParseState,
    highlight: HighlightState,
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// A theme whose "colors" are token numbers (in the red channel).
fn highlighter() -> &'static Highlighter<'static> {
    static THEME: OnceLock<Theme> = OnceLock::new();
    static HIGHLIGHTER: OnceLock<Highlighter<'static>> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| {
        let token = |token: Token| Color { r: token as u8, g: 0, b: 0, a: 255 };
        let theme = THEME.get_or_init(|| Theme {
            settings: ThemeSettings {
                foreground: Some(token(Token::Plain)),
                ..ThemeSettings::default()
            },
            scopes: RULES
                .iter()
                .map(|(selectors, t)| ThemeItem {
                    scope: ScopeSelectors::from_str(selectors).expect("valid scope selectors"),
                    style: StyleModifier {
                        foreground: Some(token(*t)),
                        background: None,
                        font_style: None,
                    },
                })
                .collect(),
            ..Theme::default()
        });
        Highlighter::new(theme)
    })
}

/// A language the editor can highlight.
#[derive(Clone, Copy)]
pub struct Language {
    syntax: &'static SyntaxReference,
}

impl Language {
    /// The language of the file at `path`, by its name or extension, or by
    /// its first line (`#!/usr/bin/env python`). `None` for plain text.
    pub fn detect(path: &Path, first_line: &str) -> Option<Language> {
        let set = syntaxes();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let extension = path.extension().and_then(|ext| ext.to_str());
        let syntax = set
            // Whole names first: Makefile, Dockerfile, .bashrc, Cargo.lock.
            .find_syntax_by_extension(name)
            .or_else(|| extension.and_then(|ext| set.find_syntax_by_extension(ext)))
            .or_else(|| extension.and_then(|ext| set.find_syntax_by_extension(&ext.to_lowercase())))
            .or_else(|| set.find_syntax_by_first_line(first_line))?;
        (syntax.name != "Plain Text").then_some(Language { syntax })
    }

    pub fn name(&self) -> &'static str {
        &self.syntax.name
    }

    /// The state at the start of a file.
    pub fn start(&self) -> State {
        State {
            parse: ParseState::new(self.syntax),
            highlight: HighlightState::new(highlighter(), ScopeStack::new()),
        }
    }

    /// Highlights `lines`, the first starting in `state`. Returns each
    /// line's spans and the state the next line starts in.
    pub fn highlight(&self, mut state: State, lines: &[String]) -> Vec<(Vec<Span>, State)> {
        let highlighter = highlighter();
        let mut text = String::new();
        lines
            .iter()
            .map(|line| {
                // These grammars match the newline too.
                text.clear();
                text.push_str(line);
                text.push('\n');
                let mut spans: Vec<Span> = Vec::new();
                if let Ok(ops) = state.parse.parse_line(&text, syntaxes()) {
                    let ranges = RangedHighlightIterator::new(&mut state.highlight, &ops, &text, highlighter);
                    for (style, _, range) in ranges {
                        let mut token = Token::ALL.get(style.foreground.r as usize).copied().unwrap_or(Token::Plain);
                        let end = range.end.min(line.len());
                        if token == Token::Plain || range.start >= end {
                            continue;
                        }
                        // Grammars give `let` and `const` the scope types get.
                        if token == Token::Type && DECLARATIONS.contains(&&line[range.start..end]) {
                            token = Token::Keyword;
                        }
                        match spans.last_mut() {
                            Some(last) if last.token == token && last.end as usize == range.start => {
                                last.end = end as u32;
                            }
                            _ => spans.push(Span {
                                start: range.start as u32,
                                end: end as u32,
                                token,
                            }),
                        }
                    }
                }
                (spans, state.clone())
            })
            .collect()
    }

    /// What starts a line comment, for ⌘/.
    pub fn line_comment(&self) -> Option<&'static str> {
        let name = self.name();
        const SLASHES: &[&str] = &[
            "Rust", "C", "C++", "C#", "Go", "Java", "JavaScript", "TypeScript", "TypeScriptReact",
            "Swift", "Kotlin", "Scala", "Dart", "Objective-C", "Objective-C++", "PHP", "Zig", "Groovy",
            "SCSS", "Sass", "Less", "Protocol Buffer", "GLSL", "HLSL", "Solidity", "D", "V", "Odin",
            "JSON (with comments)", "JSONC", "Jsonnet",
        ];
        const HASHES: &[&str] = &[
            "Python", "Ruby", "Perl", "R", "YAML", "TOML", "Makefile", "Dockerfile", "CMake", "Elixir",
            "Nix", "fish", "PowerShell", "Crystal", "Julia", "Terraform", "HCL", "nginx", "Git Ignore",
            "Git Attributes", "Git Config", "Bourne Again Shell (bash)", "Shell-Unix-Generic", "zsh",
            "Starlark", "GraphQL", "Nim", "Just", "Meson", "INI",
        ];
        if SLASHES.contains(&name) || name.starts_with("JavaScript") || name.starts_with("TypeScript") {
            Some("//")
        } else if HASHES.contains(&name) || name.contains("Shell") {
            Some("#")
        } else if ["SQL", "Lua", "Haskell", "Elm", "Ada"].contains(&name) {
            Some("--")
        } else if ["Lisp", "Clojure", "Scheme", "Racket"].contains(&name) {
            Some(";;")
        } else if ["Erlang", "LaTeX", "TeX", "MATLAB"].contains(&name) {
            Some("%")
        } else if name == "VimL" {
            Some("\"")
        } else {
            None
        }
    }
}

/// How a token looks: its color, and whether it's italic or bold.
pub fn style(token: Token, theme: &crate::ui::theme::Theme) -> (Hsla, FontStyle, FontWeight) {
    let t = &theme.tints;
    let (color, italic, bold) = match token {
        Token::Plain => (theme.foreground, false, false),
        Token::Comment => (theme.muted_foreground, true, false),
        Token::String => (t.green.soft_foreground, false, false),
        Token::Number => (t.amber.soft_foreground, false, false),
        Token::Keyword => (t.violet.soft_foreground, false, false),
        Token::Type => (t.cyan.soft_foreground, false, false),
        Token::Function => (t.blue.soft_foreground, false, false),
        Token::Property => (t.rose.soft_foreground, false, false),
        Token::Tag => (t.rose.soft_foreground, false, false),
        Token::Attribute => (t.amber.soft_foreground, false, false),
        Token::Punctuation => (theme.foreground.opacity(0.7), false, false),
        Token::Heading => (t.blue.soft_foreground, false, true),
        Token::Emphasis => (theme.foreground, true, false),
        Token::Strong => (theme.foreground, false, true),
        Token::Inserted => (t.green.soft_foreground, false, false),
        Token::Deleted => (t.destructive.soft_foreground, false, false),
    };
    (
        color,
        if italic { FontStyle::Italic } else { FontStyle::Normal },
        if bold { FontWeight::BOLD } else { FontWeight::NORMAL },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(path: &str, code: &str) -> Vec<(String, Token)> {
        let lines: Vec<String> = code.lines().map(String::from).collect();
        let language = Language::detect(Path::new(path), &lines[0]).expect("a language");
        let highlighted = language.highlight(language.start(), &lines);
        lines
            .iter()
            .zip(highlighted)
            .flat_map(|(line, (spans, _))| {
                spans
                    .into_iter()
                    .map(|span| (line[span.start as usize..span.end as usize].to_string(), span.token))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn languages_come_from_names_and_first_lines() {
        let name = |path: &str, first: &str| Language::detect(Path::new(path), first).map(|l| l.name());
        assert_eq!(name("src/main.rs", ""), Some("Rust"));
        assert_eq!(name("app.tsx", ""), Some("TypeScriptReact"));
        assert_eq!(name("Cargo.toml", ""), Some("TOML"));
        assert_eq!(name("Makefile", ""), Some("Makefile"));
        assert_eq!(name("run", "#!/usr/bin/env python3"), Some("Python"));
        assert_eq!(name("notes.txt", "hello"), None);
    }

    #[test]
    fn rust_comes_out_in_tokens() {
        let found = tokens("a.rs", "// says hi\nfn main() {\n    let n: u32 = 42;\n    println!(\"hi {n}\");\n}");
        let has = |text: &str, token: Token| found.iter().any(|(t, k)| t.contains(text) && *k == token);
        assert!(has("says hi", Token::Comment), "{found:?}");
        assert!(has("fn", Token::Keyword), "{found:?}");
        assert!(has("main", Token::Function), "{found:?}");
        assert!(has("let", Token::Keyword), "{found:?}");
        assert!(has("u32", Token::Type), "{found:?}");
        assert!(has("42", Token::Number), "{found:?}");
        assert!(has("hi", Token::String), "{found:?}");
    }

    #[test]
    fn states_carry_from_line_to_line() {
        // A block comment opened on one line colors the next.
        let found = tokens("a.c", "/* open\nstill comment */ int x;");
        assert!(found.iter().any(|(t, k)| t.contains("still comment") && *k == Token::Comment), "{found:?}");
        assert!(found.iter().any(|(t, k)| t == "int" && *k == Token::Type), "{found:?}");
    }
}

