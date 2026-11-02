//! Settings: the app's language and look, your profile (the header shows
//! it), and how canvases behave. Saved in `settings.json` in the data
//! folder; the Settings page edits them.

use gpui::{App, BorrowAppContext, Global};
use serde::{Deserialize, Serialize};

use crate::i18n::{self, Language};
use crate::services::projects::ProjectColor;

/// Light or dark.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Appearance {
    /// As the system is set.
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `None`: the system's language (when it's one the app has).
    pub language: Option<Language>,
    pub appearance: Appearance,
    /// Your name and email as the header shows them. Empty: git's
    /// (`user.name`, `user.email`).
    pub name: String,
    pub email: String,
    pub avatar_color: ProjectColor,
    /// The dots on empty canvas.
    pub dot_grid: bool,
    /// Moving and resizing windows and marks lines them up on the grid.
    pub snap_to_grid: bool,
    /// What git says your name and email are (read when the app starts).
    #[serde(skip)]
    git_name: String,
    #[serde(skip)]
    git_email: String,
    /// The system's language, checked once.
    #[serde(skip)]
    system_language: Language,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: None,
            appearance: Appearance::System,
            name: String::new(),
            email: String::new(),
            avatar_color: ProjectColor::Blue,
            dot_grid: true,
            snap_to_grid: false,
            git_name: String::new(),
            git_email: String::new(),
            system_language: Language::English,
        }
    }
}

impl Global for Settings {}

/// The canvas grid's spacing, which snapping follows (canvas units).
pub const GRID: f32 = 24.;

fn path() -> Option<std::path::PathBuf> {
    crate::services::paths::data_dir().map(|dir| dir.join("settings.json"))
}

/// `git config --global <key>`, or empty.
fn git_config(key: &str) -> String {
    std::process::Command::new("git")
        .args(["config", "--global", key])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

impl Settings {
    /// Reads the settings and applies the language. Call before the first
    /// window opens.
    pub fn init(cx: &mut App) {
        let mut settings: Settings = path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        settings.git_name = git_config("user.name");
        settings.git_email = git_config("user.email");
        settings.system_language = Language::from_system();
        i18n::set_language(settings.language());
        cx.set_global(settings);
    }

    pub fn get(cx: &App) -> &Settings {
        cx.global::<Settings>()
    }

    /// Changes the settings, saves them and shows the change everywhere.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
        let language = Settings::get(cx).language();
        cx.update_global::<Settings, _>(|settings, _| {
            change(settings);
            i18n::set_language(settings.language());
            if let Some(path) = path()
                && let Ok(json) = serde_json::to_string_pretty(settings)
            {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).ok();
                }
                if let Err(error) = std::fs::write(&path, json + "\n") {
                    eprintln!("xharness: can't save {}: {error}", path.display());
                }
            }
        });
        // Text comes from the settings as windows render: in another
        // language, render them all again (cached views too). Whoever shows
        // the rest observes the settings.
        if Settings::get(cx).language() != language {
            cx.refresh_windows();
        }
    }

    /// The language in use.
    pub fn language(&self) -> Language {
        self.language.unwrap_or(self.system_language)
    }

    /// Your name: as set here, else git's, else your account's.
    pub fn display_name(&self) -> String {
        [&self.name, &self.git_name]
            .into_iter()
            .map(|name| name.trim())
            .find(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(|| std::env::var("USER").ok().filter(|user| !user.is_empty()))
            .unwrap_or_else(|| i18n::t("You").to_string())
    }

    pub fn display_email(&self) -> String {
        if self.email.trim().is_empty() { self.git_email.clone() } else { self.email.trim().to_string() }
    }

    /// What git has, for the profile fields' placeholders.
    pub fn git_identity(&self) -> (&str, &str) {
        (&self.git_name, &self.git_email)
    }

    /// One or two letters for the avatar.
    pub fn initials(&self) -> String {
        initials(&self.display_name())
    }
}

/// The first letters of the first two words ("Ada Lovelace" → "AL"), or the
/// first two letters of a single word.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let letters: String = match words.as_slice() {
        [] => "?".into(),
        [one] => one.chars().take(2).collect(),
        [first, second, ..] => first.chars().take(1).chain(second.chars().take(1)).collect(),
    };
    letters.to_uppercase()
}

/// `value` on the grid when snapping is on.
pub fn snap(value: f32, cx: &App) -> f32 {
    if cx.try_global::<Settings>().is_some_and(|s| s.snap_to_grid) {
        (value / GRID).round() * GRID
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_come_from_the_first_two_words() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("nik"), "NI");
        assert_eq!(initials("Никита Предеин Иванович"), "НП");
        assert_eq!(initials("  "), "?");
    }

    #[test]
    fn old_files_get_defaults_for_what_they_lack() {
        let settings: Settings = serde_json::from_str(r#"{"language":"ru"}"#).unwrap();
        assert_eq!(settings.language, Some(Language::Russian));
        assert!(settings.dot_grid);
        assert_eq!(settings.appearance, Appearance::System);
    }
}
