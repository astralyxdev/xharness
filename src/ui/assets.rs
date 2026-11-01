//! Assets compiled into the binary, served to GPUI through [`AssetSource`].

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use serde::{Deserialize, Serialize};

/// Declares the icon set once: the `IconName` enum, each icon's asset path,
/// and the embedded file table. `$file` is the Lucide name, which is also the
/// SVG file name in `assets/icons/` and the name stored in saved data.
macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        /// Lucide icons (https://lucide.dev, ISC license), the icon set
        /// Astralyx UI uses.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub enum IconName {
            $(#[serde(rename = $file)] $variant,)*
        }

        impl IconName {
            pub fn path(self) -> &'static str {
                match self {
                    $(IconName::$variant => concat!("icons/", $file, ".svg"),)*
                }
            }
        }

        const FILES: &[(&str, &[u8])] = &[
            $((
                concat!("icons/", $file, ".svg"),
                include_bytes!(concat!("../../assets/icons/", $file, ".svg")),
            ),)*
        ];
    };
}

icons! {
    // Interface
    Activity => "activity",
    ArrowDown => "arrow-down",
    ArrowUpFromLine => "arrow-up-from-line",
    ArrowUp => "arrow-up",
    Check => "check",
    Cloud => "cloud",
    ColumnsThree => "columns-3",
    Copy => "copy",
    Download => "download",
    CopyPlus => "copy-plus",
    ExternalLink => "external-link",
    File => "file",
    FileCode => "file-code",
    FileImage => "file-image",
    FileText => "file-text",
    FolderTree => "folder-tree",
    Github => "github",
    GitBranch => "git-branch",
    HardDrive => "hard-drive",
    KeyRound => "key-round",
    LogOut => "log-out",
    RefreshCw => "refresh-cw",
    Search => "search",
    SlidersHorizontal => "sliders-horizontal",
    ChevronRight => "chevron-right",
    Columns => "columns-2",
    Ellipsis => "ellipsis",
    FolderOpen => "folder-open",
    Maximize => "maximize",
    Minus => "minus",
    House => "house",
    Paintbrush => "paintbrush",
    Pencil => "pencil",
    Pin => "pin",
    PinOff => "pin-off",
    Play => "play",
    Plus => "plus",
    Rows => "rows-2",
    Square => "square",
    SquareTerminal => "square-terminal",
    Store => "store",
    Trash => "trash-2",
    X => "x",
    // Project icons
    Folder => "folder",
    Box => "box",
    Rocket => "rocket",
    CodeXml => "code-xml",
    Terminal => "terminal",
    Globe => "globe",
    Database => "database",
    Zap => "zap",
    Flame => "flame",
    Leaf => "leaf",
    Star => "star",
    Heart => "heart",
    Briefcase => "briefcase",
    BookOpen => "book-open",
    Music => "music",
    Palette => "palette",
    Gamepad => "gamepad-2",
    Sparkles => "sparkles",
}

impl IconName {
    /// Icons offered when creating a project, in picker order.
    pub const PROJECT: [IconName; 18] = [
        IconName::Folder,
        IconName::Box,
        IconName::Rocket,
        IconName::CodeXml,
        IconName::Terminal,
        IconName::Globe,
        IconName::Database,
        IconName::Zap,
        IconName::Flame,
        IconName::Leaf,
        IconName::Star,
        IconName::Heart,
        IconName::Briefcase,
        IconName::BookOpen,
        IconName::Music,
        IconName::Palette,
        IconName::Gamepad,
        IconName::Sparkles,
    ];
}

/// The app's logo (full color: drawn with `img`, not `svg`). Also the
/// Dock icon.
pub const LOGO: &[u8] = include_bytes!("../../assets/logo.svg");
pub const LOGO_PATH: &str = "logo.svg";

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == LOGO_PATH {
            return Ok(Some(Cow::Borrowed(LOGO)));
        }
        Ok(FILES
            .iter()
            .find(|(file, _)| *file == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(FILES
            .iter()
            .filter(|(file, _)| file.starts_with(path))
            .map(|(file, _)| SharedString::from(*file))
            .collect())
    }
}
