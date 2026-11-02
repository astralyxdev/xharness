//! Mods: the kinds of windows a canvas can have. They all come installed;
//! the Marketplace can uninstall (and reinstall) each. An uninstalled mod
//! isn't offered under "New window", its windows close, and what it does
//! elsewhere stops (without the Editor, files open in their default app;
//! without Lino, new projects don't get lino set up).
//!
//! Which are uninstalled is saved in `mods.json` in the data directory.

pub mod browser;
pub mod editor;
pub mod explorer;
pub mod git;
pub mod lino;
pub mod sftp;
pub mod terminal;

use std::collections::BTreeSet;

use gpui::{App, BorrowAppContext, Global};
use serde::{Deserialize, Serialize};

use crate::ui::assets::IconName;
use crate::services::projects::ItemKind;

pub struct Mod {
    pub kind: ItemKind,
    pub name: &'static str,
    pub description: &'static str,
}

impl Mod {
    pub fn icon(&self) -> IconName {
        self.kind.icon()
    }
}

pub const MODS: &[Mod] = &[
    Mod {
        kind: ItemKind::Terminal,
        name: "Terminal",
        description: "Shells that keep running when the app quits, with zsh set up to tell the canvas what they're doing.",
    },
    Mod {
        kind: ItemKind::Explorer,
        name: "Explorer",
        description: "Folders as a list, like Finder: filter, sort, drag files between explorers, go to any folder.",
    },
    Mod {
        kind: ItemKind::Editor,
        name: "Editor",
        description: "Edit files with syntax highlighting, and errors, completions and definitions from language servers.",
    },
    Mod {
        kind: ItemKind::Git,
        name: "Git",
        description: "Changes, commits, branches and history with its branch graph, like GitHub Desktop.",
    },
    Mod {
        kind: ItemKind::Browser,
        name: "Browser",
        description: "Web pages with back, forward and an address bar, at the window's size or an iPhone's, iPad's or desktop's.",
    },
    Mod {
        kind: ItemKind::Sftp,
        name: "SFTP",
        description: "Your servers' files next to the project's, like FileZilla: add any number of servers, drag files across, watch the transfers.",
    },
    Mod {
        kind: ItemKind::Lino,
        name: "Lino",
        description: "lino's live index of the project, for agents: its stats, recent changes, start and stop.",
    },
];

#[derive(Default, Serialize, Deserialize)]
struct ModsFile {
    #[serde(default)]
    uninstalled: BTreeSet<ItemKind>,
}

/// Which mods are installed (a global).
#[derive(Default)]
pub struct Mods {
    uninstalled: BTreeSet<ItemKind>,
}

impl Global for Mods {}

impl Mods {
    pub fn init(cx: &mut App) {
        let uninstalled = path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|json| serde_json::from_str::<ModsFile>(&json).ok())
            .map(|file| file.uninstalled)
            .unwrap_or_default();
        cx.set_global(Mods { uninstalled });
    }

    pub fn installed(kind: ItemKind, cx: &App) -> bool {
        cx.try_global::<Mods>().is_none_or(|mods| !mods.uninstalled.contains(&kind))
    }

    /// Installs or uninstalls `kind`, and saves.
    pub fn set(kind: ItemKind, installed: bool, cx: &mut App) {
        cx.update_global::<Mods, _>(|mods, _| {
            if installed {
                mods.uninstalled.remove(&kind);
            } else {
                mods.uninstalled.insert(kind);
            }
            if let Some(path) = path() {
                let file = ModsFile { uninstalled: mods.uninstalled.clone() };
                if let Ok(json) = serde_json::to_string_pretty(&file) {
                    std::fs::write(path, json).ok();
                }
            }
        });
    }
}

fn path() -> Option<std::path::PathBuf> {
    crate::services::projects::data_dir().map(|dir| dir.join("mods.json"))
}
