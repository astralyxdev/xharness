//! Russian. Each area of the app keeps its own list (the English text, then
//! the Russian), so they can be edited side by side; the lists are merged
//! when the language is first used. Counts have three forms (see `tn`),
//! keyed by the English singular.

mod app;
mod browser;
mod canvas;
mod sftp;
mod terminal;
mod windows;

type Entries = &'static [(&'static str, &'static str)];
type Plurals = &'static [(&'static str, [&'static str; 3])];

const ALL: [(Entries, Plurals); 6] = [
    (app::ENTRIES, app::PLURALS),
    (canvas::ENTRIES, canvas::PLURALS),
    (browser::ENTRIES, browser::PLURALS),
    (windows::ENTRIES, windows::PLURALS),
    (terminal::ENTRIES, terminal::PLURALS),
    (sftp::ENTRIES, sftp::PLURALS),
];

pub fn entries() -> impl Iterator<Item = (&'static str, &'static str)> {
    ALL.iter().flat_map(|(entries, _)| entries.iter().copied())
}

pub fn plurals() -> impl Iterator<Item = (&'static str, [&'static str; 3])> {
    ALL.iter().flat_map(|(_, plurals)| plurals.iter().copied())
}
