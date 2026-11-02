//! Pages visited in Browser windows (all of them, every project), most
//! recent first: the address bar suggests from it, and a new window's start
//! page lists the latest. Saved as `browser-history.json` in the app's data
//! folder.

use std::path::PathBuf;

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

/// How many pages it keeps.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LIMIT: usize = 200;
/// How many suggestions the address bar shows.
pub const SUGGESTIONS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visit {
    pub url: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Default)]
pub struct History {
    visits: Vec<Visit>,
}

impl Global for History {}

#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    visits: Vec<Visit>,
}

fn path() -> Option<PathBuf> {
    crate::services::paths::data_dir().map(|dir| dir.join("browser-history.json"))
}

impl History {
    /// The app's history, read from disk the first time.
    pub fn get(cx: &mut App) -> &History {
        if cx.try_global::<History>().is_none() {
            let visits = path()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|json| serde_json::from_str::<HistoryFile>(&json).ok())
                .map(|file| file.visits)
                .unwrap_or_default();
            cx.set_global(History { visits });
        }
        cx.global::<History>()
    }

    /// Notes a visit to `url` (moving it to the top), and saves. (Pages
    /// only load on macOS.)
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn record(url: &str, title: &str, cx: &mut App) {
        if !worth_keeping(url) {
            return;
        }
        History::get(cx);
        let changed = cx.global_mut::<History>().add(url, title);
        if !changed {
            return;
        }
        let file = HistoryFile { visits: cx.global::<History>().visits.clone() };
        cx.background_executor()
            .spawn(async move {
                let Some(path) = path() else { return };
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).ok();
                }
                if let Ok(json) = serde_json::to_vec_pretty(&file) {
                    std::fs::write(path, json).ok();
                }
            })
            .detach();
    }

    /// Adds a visit; returns whether anything changed.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn add(&mut self, url: &str, title: &str) -> bool {
        let title = title.trim();
        if let Some(first) = self.visits.first_mut()
            && first.url == url
        {
            // The same page again (a reload, its title arriving).
            if title.is_empty() || first.title == title {
                return false;
            }
            first.title = title.to_string();
            return true;
        }
        let previous = self.visits.iter().position(|visit| visit.url == url).map(|ix| self.visits.remove(ix));
        let title = if title.is_empty() { previous.map(|visit| visit.title).unwrap_or_default() } else { title.to_string() };
        self.visits.insert(0, Visit { url: url.to_string(), title });
        self.visits.truncate(LIMIT);
        true
    }

    /// The latest visits.
    pub fn recent(&self, count: usize) -> Vec<Visit> {
        self.visits.iter().take(count).cloned().collect()
    }

    /// Visits whose address or title contains every word of `query`, most
    /// recent first.
    pub fn suggest(&self, query: &str, count: usize) -> Vec<Visit> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return self.recent(count);
        }
        self.visits
            .iter()
            .filter(|visit| {
                let (url, title) = (visit.url.to_lowercase(), visit.title.to_lowercase());
                words.iter().all(|word| url.contains(word.as_str()) || title.contains(word.as_str()))
            })
            .take(count)
            .cloned()
            .collect()
    }
}

/// Error pages and blank pages aren't worth suggesting again.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn worth_keeping(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    !url.is_empty() && !lower.starts_with("about:") && !lower.starts_with("data:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(urls: &[(&str, &str)]) -> History {
        let mut history = History::default();
        for (url, title) in urls.iter().rev() {
            history.add(url, title);
        }
        history
    }

    #[test]
    fn a_visit_moves_to_the_top_once() {
        let mut h = history(&[("https://a.com", "A"), ("https://b.com", "B")]);
        assert!(h.add("https://b.com", ""));
        let urls: Vec<&str> = h.visits.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(urls, ["https://b.com", "https://a.com"]);
        // Its title stays when the new visit hasn't one yet.
        assert_eq!(h.visits[0].title, "B");
        // The same page again changes nothing, unless its title arrives.
        assert!(!h.add("https://b.com", ""));
        assert!(h.add("https://b.com", "Bee"));
        assert_eq!(h.visits[0].title, "Bee");
    }

    #[test]
    fn it_keeps_the_latest_few_hundred() {
        let mut h = History::default();
        for ix in 0..LIMIT + 10 {
            h.add(&format!("https://site{ix}.com"), "");
        }
        assert_eq!(h.visits.len(), LIMIT);
        assert_eq!(h.visits[0].url, format!("https://site{}.com", LIMIT + 9));
    }

    #[test]
    fn suggestions_match_every_word_in_the_address_or_title() {
        let h = history(&[
            ("http://localhost:3000/admin", "Admin · Dashboard"),
            ("https://docs.rs/gpui", "gpui - Rust"),
            ("http://localhost:5173", "Vite App"),
        ]);
        let urls = |query: &str| h.suggest(query, 8).into_iter().map(|v| v.url).collect::<Vec<_>>();
        assert_eq!(urls("localhost"), ["http://localhost:3000/admin", "http://localhost:5173"]);
        assert_eq!(urls("rust GPUI"), ["https://docs.rs/gpui"]);
        assert_eq!(urls("vite 5173"), ["http://localhost:5173"]);
        assert!(urls("nothing").is_empty());
        assert_eq!(urls("").len(), 3);
        assert!(!worth_keeping("about:blank"));
    }
}
