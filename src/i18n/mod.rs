//! The app's languages: English, which the code is written in, and Russian.
//!
//! Text the app shows goes through [`t`] (`t("New terminal")`), [`tf`] for
//! text with values in it (`tf("Delete “{}”?", &[&name])`) and [`tn`] for
//! counts (`tn(3, "{} terminal", "{} terminals")`). The English text is the
//! key: a translation that's missing shows the English, and a test checks
//! that every `t("…")` in the source has one (see `ru/`).
//!
//! The language is chosen in Settings (and saved there); changing it
//! renders every window again.

use std::{
    collections::HashMap,
    fmt::Display,
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

use serde::{Deserialize, Serialize};

mod ru;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "ru")]
    Russian,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::English, Language::Russian];

    /// The language's name in itself, as a language picker shows it.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::Russian => "Русский",
        }
    }

    /// The system's language, when it's one of these.
    pub fn from_system() -> Language {
        let wanted = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.is_empty())
            .or_else(apple_language)
            .unwrap_or_default();
        if wanted.to_lowercase().starts_with("ru") { Language::Russian } else { Language::English }
    }

    fn index(self) -> u8 {
        match self {
            Language::English => 0,
            Language::Russian => 1,
        }
    }
}

/// The first of macOS's preferred languages (apps started from Finder have
/// no `LANG`).
fn apple_language() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = std::process::Command::new("defaults").args(["read", "-g", "AppleLanguages"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.split(['(', ',', ')']).map(|s| s.trim().trim_matches('"')).find(|s| !s.is_empty())?;
    Some(first.to_string())
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn language() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Language::Russian,
        _ => Language::English,
    }
}

pub fn set_language(language: Language) {
    CURRENT.store(language.index(), Ordering::Relaxed);
}

fn russian() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| ru::entries().collect())
}

fn russian_plurals() -> &'static HashMap<&'static str, [&'static str; 3]> {
    static MAP: OnceLock<HashMap<&'static str, [&'static str; 3]>> = OnceLock::new();
    MAP.get_or_init(|| ru::plurals().collect())
}

/// `en` in the app's language (`en` itself when there's no translation).
pub fn t(en: &'static str) -> &'static str {
    match language() {
        Language::English => en,
        Language::Russian => russian().get(en).copied().unwrap_or(en),
    }
}

/// `t(template)` with each `{}` replaced by the next of `args`.
pub fn tf(template: &'static str, args: &[&dyn Display]) -> String {
    fill(t(template), args)
}

/// A count: `one` (with `{}` for the number) for 1, `other` otherwise, in
/// English; Russian has three forms (1, 2–4, 5–20…), found by `one`.
pub fn tn(n: usize, one: &'static str, other: &'static str) -> String {
    let template = match language() {
        Language::English => {
            if n == 1 {
                one
            } else {
                other
            }
        }
        Language::Russian => match russian_plurals().get(one) {
            Some(forms) => forms[russian_form(n)],
            None if n == 1 => one,
            None => other,
        },
    };
    fill(template, &[&n])
}

/// Which of Russian's three plural forms goes with `n`.
fn russian_form(n: usize) -> usize {
    let (tens, ones) = (n % 100, n % 10);
    if ones == 1 && tens != 11 {
        0
    } else if (2..=4).contains(&ones) && !(12..=14).contains(&tens) {
        1
    } else {
        2
    }
}

fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut args = args.iter();
    let mut rest = template;
    while let Some(at) = rest.find("{}") {
        out.push_str(&rest[..at]);
        match args.next() {
            Some(arg) => out.push_str(&arg.to_string()),
            None => out.push_str("{}"),
        }
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, path::Path};

    #[test]
    fn russian_counts_take_the_right_form() {
        let forms: Vec<usize> = [1, 2, 4, 5, 11, 12, 14, 21, 22, 25, 101, 111].iter().map(|&n| russian_form(n)).collect();
        assert_eq!(forms, [0, 1, 1, 2, 2, 2, 2, 0, 1, 2, 0, 2]);
    }

    #[test]
    fn values_fill_the_gaps_in_order() {
        assert_eq!(fill("{} of {}", &[&1, &"two"]), "1 of two");
        assert_eq!(fill("no gaps", &[&1]), "no gaps");
        assert_eq!(fill("{} and {}", &[&1]), "1 and {}");
    }

    #[test]
    fn translations_keep_their_gaps_and_are_listed_once() {
        let mut seen = HashSet::new();
        for (en, ru) in ru::entries() {
            assert!(seen.insert(en), "translated twice: {en:?}");
            assert_eq!(en.matches("{}").count(), ru.matches("{}").count(), "{en:?} → {ru:?}");
        }
        for (one, forms) in ru::plurals() {
            for form in forms {
                assert_eq!(form.matches("{}").count(), one.matches("{}").count(), "{one:?} → {form:?}");
            }
        }
    }

    /// The string literal starting at `text` (just after its opening
    /// quote), unescaped, and where it ends.
    fn literal(text: &str) -> Option<(String, usize)> {
        let mut out = String::new();
        let mut chars = text.char_indices();
        while let Some((at, c)) = chars.next() {
            match c {
                '"' => return Some((out, at + 1)),
                '\\' => match chars.next()?.1 {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    '0' => out.push('\0'),
                    'u' => {
                        let mut hex = String::new();
                        for (_, c) in chars.by_ref() {
                            match c {
                                '{' => {}
                                '}' => break,
                                c => hex.push(c),
                            }
                        }
                        out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                    }
                    // A line continuation: the newline and the next line's
                    // indent go.
                    '\n' => {
                        let rest = chars.as_str();
                        let indent = rest.len() - rest.trim_start().len();
                        for _ in 0..rest[..indent].chars().count() {
                            chars.next();
                        }
                    }
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
        None
    }

    fn sources(dir: &Path, found: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                // The translations themselves.
                if path.ends_with("i18n") {
                    continue;
                }
                sources(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push((path.display().to_string(), std::fs::read_to_string(&path).unwrap()));
            }
        }
    }

    /// Every text the app shows has a Russian translation.
    #[test]
    fn every_text_is_translated() {
        let mut files = Vec::new();
        sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let russian = russian();
        let plurals = russian_plurals();
        let mut missing = Vec::new();
        for (path, source) in &files {
            for call in ["t(\"", "tf(\""] {
                let mut rest = source.as_str();
                while let Some(at) = rest.find(call) {
                    let before = rest[..at].chars().next_back();
                    rest = &rest[at + call.len()..];
                    // `t(` itself, not `print(` or `.at(`.
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                        continue;
                    }
                    if let Some((text, _)) = literal(rest)
                        && !russian.contains_key(text.as_str())
                    {
                        missing.push(format!("{path}: {text:?}"));
                    }
                }
            }
            // tn(n, "one", "other"): the `one` form.
            let mut rest = source.as_str();
            while let Some(at) = rest.find("tn(") {
                let before = rest[..at].chars().next_back();
                rest = &rest[at + 3..];
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                    continue;
                }
                let Some(quote) = rest.find('"') else { break };
                if rest[..quote].contains(')') {
                    continue;
                }
                if let Some((one, _)) = literal(&rest[quote + 1..])
                    && !plurals.contains_key(one.as_str())
                {
                    missing.push(format!("{path}: plural {one:?}"));
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "{} texts have no Russian translation:\n{}", missing.len(), missing.join("\n"));
    }
}
