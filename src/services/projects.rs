//! Projects: the data model and where it's saved.
//!
//! A project lives in its folder, in `.xharness/`:
//!
//! - `project.json`: what the project *is*. Name, icon, color, and its
//!   windows: titles, colors and where they sit on the canvas. Fine to
//!   commit and share.
//! - `state.json`: this machine's state. Each window's terminal session,
//!   the folder its shell was last in, and the canvas pan and zoom. Listed
//!   in `.xharness/.gitignore`.
//!
//! The app keeps a registry of projects (`projects.json` in its data
//! folder): which folders, pinned or not, open in the sidebar or not, plus
//! a copy of each name, icon and color for when a folder is unavailable.
//! Projects from before folders existed keep everything in the registry.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
};

use gpui::Hsla;
use serde::{Deserialize, Serialize};

use crate::services::files;
use crate::services::rpc;
use crate::services::server::{HostAddress, ServerKey};
use crate::ui::assets::IconName;
use crate::ui::theme::Tints;

/// Color of a project or window icon. Stored by name and resolved against
/// the theme, so it follows light/dark mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectColor {
    #[default]
    Gray,
    Blue,
    Violet,
    Cyan,
    Green,
    Amber,
    Rose,
}

impl ProjectColor {
    /// Picker order.
    pub const ALL: [ProjectColor; 7] = [
        ProjectColor::Gray,
        ProjectColor::Blue,
        ProjectColor::Violet,
        ProjectColor::Cyan,
        ProjectColor::Green,
        ProjectColor::Amber,
        ProjectColor::Rose,
    ];

    /// The color on a surface that uses `tints`. Gray has no accent, so it
    /// takes the surface's `neutral` color instead.
    pub fn resolve(self, tints: &Tints, neutral: Hsla) -> Hsla {
        match self {
            ProjectColor::Gray => neutral,
            ProjectColor::Blue => tints.blue.base,
            ProjectColor::Violet => tints.violet.base,
            ProjectColor::Cyan => tints.cyan.base,
            ProjectColor::Green => tints.green.base,
            ProjectColor::Amber => tints.amber.base,
            ProjectColor::Rose => tints.rose.base,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: u64,
    pub name: String,
    pub icon: IconName,
    pub color: ProjectColor,
    /// The project's folder. Its `.xharness/` holds the project; terminals
    /// start in it. Projects made before folders existed have none.
    #[serde(default)]
    pub folder: Option<PathBuf>,
    /// Whether the project is open in the sidebar.
    #[serde(default)]
    pub expanded: bool,
    /// Pinned projects are listed in their own group above the others.
    #[serde(default)]
    pub pinned: bool,
    /// The remote server the project is on (its key); `None`: this machine.
    /// Its folder is a path on that server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<ServerKey>,
    /// Whether the project's `.xharness` has been read (from its server).
    /// Until it has, the folder isn't written: an unreachable server's
    /// project mustn't be saved over with an empty layout.
    #[serde(skip, default = "loaded_by_default")]
    pub loaded: bool,
    /// Every window that belongs to the project, open or not. These are the
    /// rows under the project in the sidebar.
    #[serde(default)]
    pub items: Vec<ProjectItem>,
    /// Where the open windows sit on the canvas, back to front. An item is
    /// open exactly when it has a frame here.
    #[serde(default)]
    pub windows: Vec<Frame>,
    /// The part of the canvas in view, so a project reopens where you left it.
    #[serde(default)]
    pub viewport: Viewport,
}

fn loaded_by_default() -> bool {
    true
}

impl Project {
    /// The server the project's work runs on (`None`: this machine's).
    pub fn server(&self) -> Option<HostAddress> {
        self.remote.clone().map(HostAddress::Remote)
    }

    pub fn item(&self, id: u64) -> Option<&ProjectItem> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn frame(&self, item: u64) -> Option<&Frame> {
        self.windows.iter().find(|frame| frame.item == item)
    }

    pub fn is_open(&self, item: u64) -> bool {
        self.frame(item).is_some()
    }

    /// Windows that can't be removed, only closed: Lino, Git, and the first
    /// Explorer (more explorers come and go like terminals).
    pub fn is_permanent(&self, item: u64) -> bool {
        match self.item(item).map(|i| i.kind) {
            Some(ItemKind::Lino | ItemKind::Git) => true,
            Some(ItemKind::Explorer) => {
                self.folder.is_some()
                    && self.items.iter().find(|i| i.kind == ItemKind::Explorer).map(|i| i.id) == Some(item)
            }
            _ => false,
        }
    }
}

/// What a window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ItemKind {
    Terminal,
    /// The project's file explorer. Every project with a folder has one.
    Explorer,
    /// What lino (see `services/lino.rs`) is doing in the project's folder. Every
    /// project with a folder has one too.
    Lino,
    /// A file, open for editing (see `windows/editor`).
    Editor,
    /// The project's repository, like GitHub Desktop (see `windows/git.rs`).
    /// Every project with a folder has one.
    Git,
}

impl ItemKind {
    pub fn icon(self) -> IconName {
        match self {
            ItemKind::Terminal => IconName::SquareTerminal,
            ItemKind::Explorer => IconName::FolderTree,
            ItemKind::Lino => IconName::Activity,
            ItemKind::Editor => IconName::FileCode,
            ItemKind::Git => IconName::GitBranch,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Terminal => "Terminal",
            ItemKind::Explorer => "Explorer",
            ItemKind::Lino => "Lino",
            ItemKind::Editor => "Editor",
            ItemKind::Git => "Git",
        }
    }

    /// Width and height when the window opens.
    pub fn default_size(self) -> (f32, f32) {
        match self {
            ItemKind::Terminal => (DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT),
            ItemKind::Explorer => (760., 520.),
            ItemKind::Lino => (560., 600.),
            ItemKind::Editor => (760., 600.),
            ItemKind::Git => (1040., 640.),
        }
    }

    /// The windows every project with a folder has, in this order at the
    /// top of its list (see [`Project::is_permanent`]).
    pub const BUILT_IN: [ItemKind; 3] = [ItemKind::Explorer, ItemKind::Lino, ItemKind::Git];
}

/// How an Explorer window shows the folder. Saved per machine (state.json).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExplorerSettings {
    /// The folder shown, when it isn't the project's.
    pub root: Option<PathBuf>,
    pub sort: SortKey,
    pub descending: bool,
    pub folders_first: bool,
    /// Dotfiles (`.git` and `.DS_Store` are never shown).
    pub show_hidden: bool,
    /// Files matched by `.gitignore`.
    pub show_ignored: bool,
    /// Columns shown after Name, in order.
    pub columns: Vec<ExplorerColumn>,
    /// Open folders, relative to the project folder.
    pub expanded: Vec<PathBuf>,
}

impl Default for ExplorerSettings {
    fn default() -> Self {
        Self {
            root: None,
            sort: SortKey::Name,
            descending: false,
            folders_first: true,
            show_hidden: true,
            show_ignored: true,
            columns: ExplorerColumn::ALL.to_vec(),
            expanded: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortKey {
    Name,
    Kind,
    Size,
    Modified,
}

/// Explorer columns besides Name (which is always shown).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExplorerColumn {
    Kind,
    Size,
    Modified,
}

impl ExplorerColumn {
    pub const ALL: [ExplorerColumn; 3] = [ExplorerColumn::Kind, ExplorerColumn::Size, ExplorerColumn::Modified];

    pub fn label(self) -> &'static str {
        match self {
            ExplorerColumn::Kind => "Kind",
            ExplorerColumn::Size => "Size",
            ExplorerColumn::Modified => "Modified",
        }
    }

    pub fn sort_key(self) -> SortKey {
        match self {
            ExplorerColumn::Kind => SortKey::Kind,
            ExplorerColumn::Size => SortKey::Size,
            ExplorerColumn::Modified => SortKey::Modified,
        }
    }
}

/// A window in a project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectItem {
    pub id: u64,
    pub kind: ItemKind,
    pub title: String,
    #[serde(default)]
    pub color: ProjectColor,
    /// The terminal session (in the session host) behind this window. It
    /// outlives the app, so reopening the app finds the same shell.
    #[serde(default = "new_session_id")]
    pub session: String,
    /// The folder the shell was last in, where a new shell starts if the
    /// session is gone (after a reboot).
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// How an Explorer window shows the folder (Explorer windows only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explorer: Option<ExplorerSettings>,
    /// The file an Editor window shows (absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// An open window's place on the canvas, in canvas units (1 unit = 1px at
/// 100% zoom).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub item: u64,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Frame {
    fn overlaps(&self, other: &Frame) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// The view onto a canvas: `x`/`y` is the canvas point at the view's
/// top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub zoom: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            x: -48.,
            y: -48.,
            zoom: 1.,
        }
    }
}

/// Size of a newly opened window: about 80 × 20 terminal cells.
pub const DEFAULT_WINDOW_WIDTH: f32 = 660.;
pub const DEFAULT_WINDOW_HEIGHT: f32 = 440.;
pub const MIN_WINDOW_WIDTH: f32 = 320.;
pub const MIN_WINDOW_HEIGHT: f32 = 200.;
/// Space left between windows that open next to each other.
pub const WINDOW_GAP: f32 = 24.;
pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 4.;

/// Where to open a window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Placement {
    /// To the right of this window, top edges aligned.
    RightOf(u64),
    /// Under this window, left edges aligned.
    Below(u64),
    /// With its top-left corner here.
    At { x: f32, y: f32 },
}

// Files ------------------------------------------------------------------------

/// The folder in a project's folder that holds the project.
pub const PROJECT_DIR: &str = ".xharness";
const PROJECT_FILE: &str = "project.json";
const STATE_FILE: &str = "state.json";
const GITIGNORE: &str = "# Written by xharness. state.json is this machine's: terminal sessions\n\
                         # and folders. project.json is the shareable layout.\n\
                         state.json\n";
const FILE_VERSION: u32 = 1;

/// The app's list of projects.
#[derive(Deserialize)]
struct RegistryFile {
    #[allow(dead_code)]
    version: u32,
    /// Never decreases, so a deleted project's or window's id is never
    /// reused.
    #[serde(default)]
    next_id: u64,
    projects: Vec<Project>,
}

#[derive(Serialize)]
struct RegistryFileRef<'a> {
    version: u32,
    next_id: u64,
    projects: Vec<RegistryEntry<'a>>,
}

/// A project in the registry. Projects with a folder are saved there, so
/// only these fields are kept here; ones without keep their windows here.
#[derive(Serialize)]
struct RegistryEntry<'a> {
    id: u64,
    name: &'a str,
    icon: IconName,
    color: ProjectColor,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<&'a ServerKey>,
    expanded: bool,
    pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<&'a [ProjectItem]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    windows: Option<&'a [Frame]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    viewport: Option<&'a Viewport>,
}

/// `.xharness/project.json`.
#[derive(Serialize, Deserialize)]
struct ProjectFile {
    version: u32,
    name: String,
    icon: IconName,
    color: ProjectColor,
    #[serde(default)]
    windows: Vec<WindowEntry>,
}

/// A window in `project.json`; `frame` is `None` while it's closed.
#[derive(Serialize, Deserialize)]
struct WindowEntry {
    id: u64,
    kind: ItemKind,
    title: String,
    #[serde(default)]
    color: ProjectColor,
    /// An Editor's file: relative to the project folder when it's inside
    /// it (so the layout travels with the folder), else absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    frame: Option<FrameEntry>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct FrameEntry {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    /// Stacking order: higher is in front.
    #[serde(default)]
    z: usize,
}

/// `.xharness/state.json`.
#[derive(Default, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    #[serde(default)]
    viewport: Viewport,
    /// By window id (as a string, for JSON).
    #[serde(default)]
    windows: BTreeMap<String, WindowState>,
}

#[derive(Serialize, Deserialize)]
struct WindowState {
    session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    explorer: Option<ExplorerSettings>,
}

/// What a folder's `.xharness` says, for offering to open it.
pub struct ExistingProject {
    pub name: String,
    pub icon: IconName,
    pub color: ProjectColor,
    pub windows: usize,
}

/// Reads the project in `folder`'s `.xharness`, if there is one.
pub fn existing_project(folder: &Path) -> Option<ExistingProject> {
    let file = read_project_file(folder).ok()??;
    Some(ExistingProject {
        name: file.name,
        icon: file.icon,
        color: file.color,
        windows: file.windows.len(),
    })
}

fn read_project_file(folder: &Path) -> io::Result<Option<ProjectFile>> {
    let path = folder.join(PROJECT_DIR).join(PROJECT_FILE);
    match files::read(&path).map_err(io::Error::other)? {
        Some(json) => serde_json::from_str(&json).map(Some).map_err(io::Error::other),
        None => Ok(None),
    }
}

/// Reads `folder/.xharness` into `project`. Returns `false` if there's none.
fn load_from_folder(project: &mut Project, folder: &Path) -> io::Result<bool> {
    let Some(file) = read_project_file(folder)? else {
        return Ok(false);
    };
    // A missing or broken state file just means a fresh start on this
    // machine: new sessions, default view.
    let state: StateFile = files::read(&folder.join(PROJECT_DIR).join(STATE_FILE))
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();

    project.name = file.name;
    project.icon = file.icon;
    project.color = file.color;
    project.viewport = state.viewport;
    project.items.clear();
    project.windows.clear();
    let mut frames = Vec::new();
    for window in file.windows {
        let local = state.windows.get(&window.id.to_string());
        project.items.push(ProjectItem {
            id: window.id,
            kind: window.kind,
            title: window.title,
            color: window.color,
            session: local.map_or_else(new_session_id, |s| s.session.clone()),
            cwd: local.and_then(|s| s.cwd.clone()),
            explorer: local
                .and_then(|s| s.explorer.clone())
                .or_else(|| (window.kind == ItemKind::Explorer).then(ExplorerSettings::default)),
            path: window.path.map(|path| folder.join(path)),
        });
        if let Some(frame) = window.frame {
            frames.push((
                frame.z,
                Frame {
                    item: window.id,
                    x: frame.x,
                    y: frame.y,
                    width: frame.width,
                    height: frame.height,
                },
            ));
        }
    }
    // Windows are listed in sidebar order; the canvas wants back to front.
    frames.sort_by_key(|(z, _)| *z);
    project.windows = frames.into_iter().map(|(_, frame)| frame).collect();
    Ok(true)
}

/// `project.json` and `state.json` for a project, as file contents.
fn folder_files(project: &Project) -> io::Result<(Vec<u8>, Vec<u8>)> {
    // Windows in sidebar order; each open one carries its stacking order.
    let windows = project
        .items
        .iter()
        .map(|item| WindowEntry {
            id: item.id,
            kind: item.kind,
            title: item.title.clone(),
            color: item.color,
            path: item.path.as_ref().map(|path| match &project.folder {
                Some(folder) => path.strip_prefix(folder).unwrap_or(path).to_path_buf(),
                None => path.clone(),
            }),
            frame: project
                .windows
                .iter()
                .position(|f| f.item == item.id)
                .map(|z| {
                    let f = project.windows[z];
                    FrameEntry {
                        x: f.x,
                        y: f.y,
                        width: f.width,
                        height: f.height,
                        z,
                    }
                }),
        })
        .collect();
    let file = ProjectFile {
        version: FILE_VERSION,
        name: project.name.clone(),
        icon: project.icon,
        color: project.color,
        windows,
    };
    let state = StateFile {
        version: FILE_VERSION,
        viewport: project.viewport,
        windows: project
            .items
            .iter()
            .map(|item| {
                (
                    item.id.to_string(),
                    WindowState {
                        session: item.session.clone(),
                        cwd: item.cwd.clone(),
                        explorer: item.explorer.clone(),
                    },
                )
            })
            .collect(),
    };
    Ok((pretty_json(&file)?, pretty_json(&state)?))
}

/// Indented JSON with a trailing newline, as people expect files to look.
fn pretty_json(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes the app's own file `path` via a temp file and a rename, so a
/// crash mid-write can't leave it truncated.
fn write_here(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    fs::write(&temp, contents)?;
    fs::rename(&temp, path)
}

/// Writes `contents` to `path` via a temp file and a rename, so a crash
/// mid-write can't leave a truncated file. The project's server does it
/// (see `files.rs`), where the project is.
fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    files::write(path, &String::from_utf8_lossy(contents)).map_err(io::Error::other)
}

// The store --------------------------------------------------------------------

/// The projects. Most changes save at once; live ones (dragging, panning,
/// a shell changing folder) are saved by whoever makes them, with
/// [`ProjectStore::save`], once they settle.
pub struct ProjectStore {
    /// The registry file. `None` disables saving: no data directory, or an
    /// existing file that couldn't be read and mustn't be overwritten.
    path: Option<PathBuf>,
    next_id: u64,
    projects: Vec<Project>,
    /// What we last wrote to each file, so saving skips unchanged ones (and
    /// doesn't touch every project's folder on every change).
    written: RefCell<HashMap<PathBuf, Vec<u8>>>,
}

impl ProjectStore {
    pub fn load() -> Self {
        let Some(path) = data_dir().map(|dir| dir.join("projects.json")) else {
            eprintln!("xharness: no home directory; projects won't be saved");
            return Self::unsaved();
        };
        match Self::load_from(&path) {
            Ok(store) => store,
            Err(error) => {
                eprintln!(
                    "xharness: can't read {}: {error}; projects won't be saved",
                    path.display()
                );
                Self::unsaved()
            }
        }
    }

    fn unsaved() -> Self {
        Self {
            path: None,
            next_id: 1,
            projects: Vec::new(),
            written: RefCell::default(),
        }
    }

    /// Loads the registry at `path`, and each project from its folder.
    fn load_from(path: &Path) -> io::Result<Self> {
        let Loaded { next_id, mut projects } = read_registry(path)?;
        let mut to_write = false;
        for project in &mut projects {
            let Some(folder) = project.folder.clone() else {
                continue;
            };
            let server = project.server();
            match rpc::on(server.as_ref(), || load_from_folder(project, &folder)) {
                Ok(true) => {}
                // Not written there yet (a project from before `.xharness`):
                // the registry's copy stands, and goes to the folder now.
                Ok(false) => to_write = true,
                // Unreadable (or its server unreachable): keep the registry's
                // copy for now; saving won't touch the folder until it's
                // been read (see `reload`).
                Err(error) => {
                    eprintln!("xharness: can't read {}: {error}", folder.join(PROJECT_DIR).display());
                    project.loaded = false;
                }
            }
        }
        let mut store = Self {
            path: Some(path.to_path_buf()),
            next_id,
            projects,
            written: RefCell::default(),
        };
        store.fix_ids();
        store.projects.iter_mut().for_each(sanitize);
        if store.ensure_built_ins() {
            to_write = true;
        }
        if to_write {
            store.save();
        }
        Ok(store)
    }

    /// Gives every project with a folder its built-in windows (closed until
    /// clicked), first in the project's list in [`ItemKind::BUILT_IN`]
    /// order. Returns whether any project changed.
    fn ensure_built_ins(&mut self) -> bool {
        let mut changed = false;
        for index in 0..self.projects.len() {
            if self.projects[index].folder.is_none() {
                continue;
            }
            for (slot, kind) in ItemKind::BUILT_IN.into_iter().enumerate() {
                let position = self.projects[index].items.iter().position(|item| item.kind == kind);
                match position {
                    Some(position) if position == slot => continue,
                    Some(position) => {
                        let item = self.projects[index].items.remove(position);
                        self.projects[index].items.insert(slot, item);
                    }
                    None => {
                        let id = self.take_id();
                        self.projects[index].items.insert(
                            slot,
                            ProjectItem {
                                id,
                                kind,
                                title: kind.label().to_string(),
                                color: ProjectColor::default(),
                                session: new_session_id(),
                                cwd: None,
                                explorer: (kind == ItemKind::Explorer).then(ExplorerSettings::default),
                                path: None,
                            },
                        );
                    }
                }
                changed = true;
            }
        }
        changed
    }

    /// Ids must be unique across the app. Folders can come from anywhere
    /// (copied, cloned), so renumber any window whose id is already taken.
    fn fix_ids(&mut self) {
        let mut seen: HashSet<u64> = self.projects.iter().map(|p| p.id).collect();
        let mut next_id = self.next_id;
        for project in &self.projects {
            for item in &project.items {
                next_id = next_id.max(item.id + 1);
            }
            next_id = next_id.max(project.id + 1);
        }
        for project in &mut self.projects {
            for index in 0..project.items.len() {
                let id = project.items[index].id;
                if seen.insert(id) {
                    continue;
                }
                let fresh = next_id;
                next_id += 1;
                seen.insert(fresh);
                project.items[index].id = fresh;
                for frame in project.windows.iter_mut().filter(|f| f.item == id) {
                    frame.item = fresh;
                }
            }
        }
        self.next_id = next_id;
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn get(&self, id: u64) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    // Projects ---------------------------------------------------------------

    /// Adds a project. If `folder` already has an `.xharness`, its windows
    /// come back with it; `name`, `icon` and `color` are what the dialog
    /// showed.
    pub fn create(
        &mut self,
        name: String,
        icon: IconName,
        color: ProjectColor,
        folder: Option<PathBuf>,
        remote: Option<ServerKey>,
    ) {
        let id = self.take_id();
        let mut project = Project {
            id,
            name: String::new(),
            icon,
            color,
            folder: folder.clone(),
            remote,
            loaded: true,
            expanded: true,
            pinned: false,
            items: Vec::new(),
            windows: Vec::new(),
            viewport: Viewport::default(),
        };
        let server = project.server();
        if let Some(folder) = &folder
            && let Err(error) = rpc::on(server.as_ref(), || load_from_folder(&mut project, folder))
        {
            eprintln!("xharness: can't read {}: {error}", folder.join(PROJECT_DIR).display());
            project.loaded = false;
        }
        (project.name, project.icon, project.color) = (name, icon, color);
        self.projects.push(project);
        self.fix_ids();
        self.ensure_built_ins();
        self.save();
    }

    /// Reads the projects on `server` that couldn't be read before (it was
    /// unreachable). Returns whether any were.
    pub fn reload(&mut self, server: Option<&HostAddress>) -> bool {
        let mut changed = false;
        for index in 0..self.projects.len() {
            let project = &mut self.projects[index];
            if project.loaded || project.server().as_ref() != server {
                continue;
            }
            let Some(folder) = project.folder.clone() else {
                continue;
            };
            let (name, icon, color) = (project.name.clone(), project.icon, project.color);
            if rpc::on(server, || load_from_folder(project, &folder)).is_ok() {
                // The app's name, icon and color for it stand.
                (project.name, project.icon, project.color) = (name, icon, color);
                project.loaded = true;
                changed = true;
            }
        }
        if changed {
            self.fix_ids();
            self.ensure_built_ins();
            self.save();
        }
        changed
    }

    pub fn update(
        &mut self,
        id: u64,
        name: String,
        icon: IconName,
        color: ProjectColor,
        folder: Option<PathBuf>,
    ) {
        self.modify(id, |project| {
            project.name = name;
            project.icon = icon;
            project.color = color;
            project.folder = folder;
        });
        // A project that just got a folder gets its Explorer and Lino.
        if self.ensure_built_ins() {
            self.save();
        }
    }

    pub fn rename(&mut self, id: u64, name: String) {
        self.modify(id, |project| project.name = name);
    }

    pub fn set_expanded(&mut self, id: u64, expanded: bool) {
        self.modify(id, |project| project.expanded = expanded);
    }

    pub fn set_pinned(&mut self, id: u64, pinned: bool) {
        self.modify(id, |project| project.pinned = pinned);
    }

    /// Removes the project from the app. Its `.xharness` stays in its
    /// folder, so adding the folder again brings it back.
    pub fn delete(&mut self, id: u64) {
        let before = self.projects.len();
        self.projects.retain(|p| p.id != id);
        if self.projects.len() != before {
            self.save();
        }
    }

    pub fn is_expanded(&self, id: u64) -> bool {
        self.get(id).is_some_and(|p| p.expanded)
    }

    // Windows ----------------------------------------------------------------

    /// Adds a window to `project` and opens it. Returns the new item's id.
    pub fn add_item(&mut self, project: u64, kind: ItemKind, placement: Placement) -> Option<u64> {
        self.get(project)?;
        let id = self.take_id();
        let project = self.project_mut(project)?;
        let number = project.items.iter().filter(|item| item.kind == kind).count() + 1;
        project.items.push(ProjectItem {
            id,
            kind,
            title: format!("{} {number}", kind.label()),
            color: ProjectColor::default(),
            session: new_session_id(),
            cwd: None,
            explorer: (kind == ItemKind::Explorer).then(ExplorerSettings::default),
            path: None,
        });
        place(project, id, placement);
        self.save();
        Some(id)
    }

    /// Adds another Explorer window (titled "Explorer 2", …) and opens it.
    pub fn add_explorer(&mut self, project: u64, settings: ExplorerSettings, placement: Placement) -> Option<u64> {
        let id = self.add_item(project, ItemKind::Explorer, placement)?;
        if let Some(item) = self.item_mut(project, id) {
            item.explorer = Some(settings);
        }
        self.save();
        Some(id)
    }

    /// Opens `path` in an Editor window: the one it's already open in, or
    /// a new one. Returns the window's id.
    pub fn open_file(&mut self, project: u64, path: PathBuf, placement: Placement) -> Option<u64> {
        let existing = self
            .get(project)?
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Editor && item.path.as_ref() == Some(&path))
            .map(|item| item.id);
        if let Some(id) = existing {
            self.open_item(project, id, placement);
            return Some(id);
        }
        let id = self.take_id();
        let project = self.project_mut(project)?;
        // Named by its path in the project, like `src/main.rs`.
        let title = match &project.folder {
            Some(folder) if path.starts_with(folder) => path.strip_prefix(folder).unwrap_or(&path).display().to_string(),
            _ => crate::ui::display_path(&path),
        };
        project.items.push(ProjectItem {
            id,
            kind: ItemKind::Editor,
            title,
            color: ProjectColor::default(),
            session: new_session_id(),
            cwd: None,
            explorer: None,
            path: Some(path),
        });
        place(project, id, placement);
        self.save();
        Some(id)
    }

    /// Opens a closed window at its default size. Does nothing if it's open.
    pub fn open_item(&mut self, project: u64, item: u64, placement: Placement) {
        self.modify(project, |project| {
            if project.item(item).is_some() && !project.is_open(item) {
                place(project, item, placement);
            }
        });
    }

    /// Takes a window off the canvas. It stays in the project, closed.
    pub fn close_item(&mut self, project: u64, item: u64) {
        self.modify(project, |project| project.windows.retain(|f| f.item != item));
    }

    /// Removes a window from the project entirely. Permanent ones (the
    /// first Explorer, Lino) stay: every project has them.
    pub fn remove_item(&mut self, project: u64, item: u64) {
        if self.get(project).is_some_and(|p| p.is_permanent(item)) {
            return;
        }
        self.modify(project, |project| {
            project.windows.retain(|f| f.item != item);
            project.items.retain(|i| i.id != item);
        });
    }

    pub fn update_item(&mut self, project: u64, item: u64, title: String, color: ProjectColor) {
        self.modify(project, |project| {
            if let Some(item) = project.items.iter_mut().find(|i| i.id == item) {
                item.title = title;
                item.color = color;
            }
        });
    }

    /// Replaces an Explorer window's view settings. Doesn't save (sorting
    /// and opening folders happen in quick succession).
    pub fn set_explorer_settings(&mut self, project: u64, item: u64, settings: ExplorerSettings) {
        if let Some(item) = self.item_mut(project, item) {
            item.explorer = Some(settings);
        }
    }

    /// Records the folder a window's shell is in. Returns whether it changed.
    /// Doesn't save (shells change folder often).
    pub fn set_item_cwd(&mut self, project: u64, item: u64, cwd: PathBuf) -> bool {
        let Some(item) = self.item_mut(project, item) else {
            return false;
        };
        if item.cwd.as_ref() == Some(&cwd) {
            return false;
        }
        item.cwd = Some(cwd);
        true
    }

    /// Moves and/or resizes an open window, keeping it at least the minimum
    /// size. Doesn't save: it's called on every mouse move of a drag.
    pub fn set_frame(&mut self, project: u64, item: u64, x: f32, y: f32, width: f32, height: f32) {
        if let Some(frame) = self
            .project_mut(project)
            .and_then(|p| p.windows.iter_mut().find(|f| f.item == item))
        {
            frame.x = x;
            frame.y = y;
            frame.width = width.max(MIN_WINDOW_WIDTH);
            frame.height = height.max(MIN_WINDOW_HEIGHT);
        }
    }

    /// Puts a window in front of the others. Returns whether it moved.
    /// Doesn't save, like [`Self::set_frame`].
    pub fn raise(&mut self, project: u64, item: u64) -> bool {
        let Some(project) = self.project_mut(project) else {
            return false;
        };
        let Some(index) = project.windows.iter().position(|f| f.item == item) else {
            return false;
        };
        if index + 1 == project.windows.len() {
            return false;
        }
        let frame = project.windows.remove(index);
        project.windows.push(frame);
        true
    }

    /// Pans and zooms, clamping the zoom. Doesn't save, like
    /// [`Self::set_frame`].
    pub fn set_viewport(&mut self, project: u64, viewport: Viewport) {
        if let Some(project) = self.project_mut(project) {
            project.viewport = Viewport {
                zoom: viewport.zoom.clamp(MIN_ZOOM, MAX_ZOOM),
                ..viewport
            };
        }
    }

    // Internals --------------------------------------------------------------

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn project_mut(&mut self, id: u64) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.id == id)
    }

    fn item_mut(&mut self, project: u64, item: u64) -> Option<&mut ProjectItem> {
        self.project_mut(project)?
            .items
            .iter_mut()
            .find(|i| i.id == item)
    }

    /// Applies `change` to the project and saves if anything changed.
    fn modify(&mut self, id: u64, change: impl FnOnce(&mut Project)) {
        let Some(project) = self.project_mut(id) else {
            return;
        };
        let before = project.clone();
        change(project);
        if *project != before {
            self.save();
        }
    }

    /// Writes the registry and every project's `.xharness` that changed.
    pub fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let registry = RegistryFileRef {
            version: FILE_VERSION,
            next_id: self.next_id,
            projects: self
                .projects
                .iter()
                .map(|p| {
                    let inline = p.folder.is_none();
                    RegistryEntry {
                        id: p.id,
                        name: &p.name,
                        icon: p.icon,
                        color: p.color,
                        folder: p.folder.as_deref(),
                        remote: p.remote.as_ref(),
                        expanded: p.expanded,
                        pinned: p.pinned,
                        items: inline.then_some(p.items.as_slice()),
                        windows: inline.then_some(p.windows.as_slice()),
                        viewport: inline.then_some(&p.viewport),
                    }
                })
                .collect(),
        };
        // (where, contents, which server); the registry is this app's.
        let mut files: Vec<(PathBuf, Vec<u8>, Option<HostAddress>)> = Vec::new();
        // The app's own list is written here.
        match pretty_json(&registry) {
            Ok(json) => {
                let mut written = self.written.borrow_mut();
                if written.get(path) != Some(&json) {
                    match write_here(path, &json) {
                        Ok(()) => {
                            written.insert(path.clone(), json);
                        }
                        Err(error) => eprintln!("xharness: can't save {}: {error}", path.display()),
                    }
                }
            }
            Err(error) => eprintln!("xharness: can't save projects: {error}"),
        }
        for project in &self.projects {
            let Some(folder) = &project.folder else {
                continue;
            };
            if !project.loaded {
                continue;
            }
            let dir = folder.join(PROJECT_DIR);
            let server = project.server();
            match folder_files(project) {
                Ok((project_json, state_json)) => {
                    files.push((dir.join(PROJECT_FILE), project_json, server.clone()));
                    files.push((dir.join(STATE_FILE), state_json, server.clone()));
                    files.push((dir.join(".gitignore"), GITIGNORE.as_bytes().to_vec(), server));
                }
                Err(error) => eprintln!("xharness: can't save {}: {error}", dir.display()),
            }
        }

        let mut written = self.written.borrow_mut();
        for (path, contents, server) in files {
            let cache_key = match &server {
                Some(HostAddress::Remote(key)) => PathBuf::from(format!("{}:{}", key.address(), path.display())),
                _ => path.clone(),
            };
            if written.get(&cache_key) == Some(&contents) {
                continue;
            }
            // A project whose folder is gone (deleted, unmounted) can't be
            // saved; it stays as it is in memory.
            if server.is_none() && !path.parent().and_then(Path::parent).is_some_and(Path::is_dir) {
                continue;
            }
            match rpc::on(server.as_ref(), || write_atomically(&path, &contents)) {
                Ok(()) => {
                    written.insert(cache_key, contents);
                }
                Err(error) => eprintln!("xharness: can't save {}: {error}", path.display()),
            }
        }
    }
}

/// Opens `item` at the default size. Next to another window, it slides
/// further right (or down) until it's clear of every open window.
fn place(project: &mut Project, item: u64, placement: Placement) {
    let anchor = |id: u64| project.frame(id).copied();
    let (width, height) = project
        .item(item)
        .map_or((DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT), |i| i.kind.default_size());
    let mut frame = Frame {
        item,
        x: 0.,
        y: 0.,
        width,
        height,
    };
    let mut step_down = false;
    match placement {
        Placement::RightOf(id) => match anchor(id) {
            Some(a) => (frame.x, frame.y) = (a.x + a.width + WINDOW_GAP, a.y),
            None => (frame.x, frame.y) = right_of_everything(project),
        },
        Placement::Below(id) => match anchor(id) {
            Some(a) => {
                (frame.x, frame.y) = (a.x, a.y + a.height + WINDOW_GAP);
                frame.width = a.width;
                step_down = true;
            }
            None => (frame.x, frame.y) = right_of_everything(project),
        },
        Placement::At { x, y } => (frame.x, frame.y) = (x, y),
    }
    // Each step clears the window in the way, so this ends after at most
    // one step per open window.
    while let Some(blocking) = project.windows.iter().find(|other| other.overlaps(&frame)) {
        if step_down {
            frame.y = blocking.y + blocking.height + WINDOW_GAP;
        } else {
            frame.x = blocking.x + blocking.width + WINDOW_GAP;
        }
    }
    project.windows.push(frame);
}

fn right_of_everything(project: &Project) -> (f32, f32) {
    let right = project
        .windows
        .iter()
        .map(|f| f.x + f.width)
        .fold(None, |max: Option<f32>, r| Some(max.map_or(r, |m| m.max(r))));
    let top = project.windows.iter().map(|f| f.y).fold(f32::INFINITY, f32::min);
    match right {
        Some(right) => (right + WINDOW_GAP, top),
        None => (0., 0.),
    }
}

/// Drops frames for missing items (and duplicates), so a hand-edited file
/// can't put ghosts on the canvas.
fn sanitize(project: &mut Project) {
    let mut seen = Vec::new();
    let items: Vec<u64> = project.items.iter().map(|item| item.id).collect();
    project.windows.retain(|frame| {
        let keep = items.contains(&frame.item) && !seen.contains(&frame.item);
        seen.push(frame.item);
        keep
    });
    project.viewport.zoom = project.viewport.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
}

struct Loaded {
    next_id: u64,
    projects: Vec<Project>,
}

fn read_registry(path: &Path) -> io::Result<Loaded> {
    let empty = Loaded {
        next_id: 1,
        projects: Vec::new(),
    };
    // The app's own list, read here (it says which server each project is on).
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(empty),
        Err(error) => return Err(error),
    };
    match serde_json::from_str::<RegistryFile>(&json) {
        Ok(file) => Ok(Loaded {
            next_id: file.next_id.max(1),
            projects: file.projects,
        }),
        Err(error) => {
            // Keep the unreadable file rather than overwriting it on the next
            // save.
            let backup = path.with_extension("json.bak");
            fs::rename(path, &backup)?;
            eprintln!(
                "xharness: {} was invalid ({error}); moved it to {}",
                path.display(),
                backup.display()
            );
            Ok(empty)
        }
    }
}

pub use crate::services::paths::data_dir;

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty directory under the system temp dir.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xharness-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A store saving its registry to `dir/projects.json`, as `load()`
    /// would build it.
    fn store_in(dir: &Path) -> ProjectStore {
        ProjectStore::load_from(&dir.join("projects.json")).unwrap()
    }

    /// A store with one project (id 1) in `dir/project`.
    fn canvas_store(name: &str) -> (ProjectStore, PathBuf) {
        let dir = temp_dir(name);
        let folder = dir.join("project");
        fs::create_dir_all(&folder).unwrap();
        let mut store = store_in(&dir);
        store.create("P".into(), IconName::Box, ProjectColor::Blue, Some(folder), None);
        (store, dir)
    }

    fn frame(store: &ProjectStore, item: u64) -> Frame {
        *store.get(1).unwrap().frame(item).unwrap()
    }

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn terminals(store: &ProjectStore) -> Vec<&ProjectItem> {
        store.get(1).unwrap().items.iter().filter(|i| i.kind == ItemKind::Terminal).collect()
    }

    #[test]
    fn every_folder_project_has_its_built_in_windows_first() {
        let (mut store, dir) = canvas_store("explorer");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 0., y: 0. }).unwrap();
        let project = store.get(1).unwrap();
        assert_eq!(project.items[0].kind, ItemKind::Explorer);
        assert_eq!((project.items[1].kind, project.items[1].title.as_str()), (ItemKind::Lino, "Lino"));
        assert_eq!(project.items[2].kind, ItemKind::Git);
        assert_eq!(project.items[3].id, a);
        let (explorer, lino) = (project.items[0].id, project.items[1].id);
        assert!(!project.is_open(explorer) && !project.is_open(lino), "closed until clicked");

        // They can't be removed, and come back first after a reload.
        store.remove_item(1, explorer);
        store.remove_item(1, lino);
        assert_eq!(store.get(1).unwrap().items[0].id, explorer);
        assert_eq!(store.get(1).unwrap().items[1].id, lino);
        let mut settings = ExplorerSettings::default();
        settings.sort = SortKey::Modified;
        settings.columns = vec![ExplorerColumn::Size];
        settings.expanded = vec![PathBuf::from("src")];
        store.set_explorer_settings(1, explorer, settings.clone());
        store.open_item(1, explorer, Placement::At { x: 0., y: 0. });
        let frame = *store.get(1).unwrap().frame(explorer).unwrap();
        assert_eq!((frame.width, frame.height), ItemKind::Explorer.default_size());
        store.save();
        let reloaded = store_in(&dir);
        let item = &reloaded.get(1).unwrap().items[0];
        assert_eq!((item.id, item.explorer.as_ref()), (explorer, Some(&settings)));

        // Projects without a folder have none.
        store.create("Loose".into(), IconName::Box, ProjectColor::Blue, None, None);
        assert!(store.projects().last().unwrap().items.is_empty());
    }

    #[test]
    fn projects_live_in_their_folder() {
        let (mut store, dir) = canvas_store("folder-files");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 10., y: 20. }).unwrap();
        let b = store.add_item(1, ItemKind::Terminal, Placement::RightOf(a)).unwrap();
        store.update_item(1, b, "Server".into(), ProjectColor::Green);
        store.close_item(1, b);
        store.set_item_cwd(1, a, PathBuf::from("/tmp"));
        store.set_viewport(1, Viewport { x: 5., y: 6., zoom: 0.5 });
        store.save();

        let xharness = dir.join("project").join(PROJECT_DIR);
        // project.json: the shareable part.
        let project = read_json(&xharness.join(PROJECT_FILE));
        assert_eq!(project["name"], "P");
        assert_eq!(project["windows"][0]["kind"], "explorer");
        assert_eq!(project["windows"][1]["kind"], "lino");
        assert_eq!(project["windows"][2]["kind"], "git");
        assert_eq!(project["windows"][3]["title"], "Terminal 1");
        assert_eq!(project["windows"][3]["frame"]["x"], 10.);
        assert_eq!(project["windows"][4]["title"], "Server");
        assert_eq!(project["windows"][4]["color"], "green");
        assert!(project["windows"][4].get("frame").is_none(), "closed");
        assert!(project["windows"][3].get("session").is_none(), "not machine state");
        // state.json: this machine's.
        let state = read_json(&xharness.join(STATE_FILE));
        let a_state = &state["windows"][a.to_string()];
        assert_eq!(a_state["cwd"], "/tmp");
        assert_eq!(a_state["session"].as_str().unwrap().len(), 32);
        assert_eq!(state["viewport"]["zoom"], 0.5);
        assert!(fs::read_to_string(xharness.join(".gitignore")).unwrap().contains("state.json"));
        // The registry only points at the folder.
        let registry = read_json(&dir.join("projects.json"));
        assert!(registry["projects"][0].get("items").is_none());
        assert!(registry["projects"][0]["folder"].as_str().unwrap().ends_with("project"));
    }

    #[test]
    fn everything_comes_back_after_a_restart() {
        let (mut store, dir) = canvas_store("restart");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 10., y: 20. }).unwrap();
        let b = store.add_item(1, ItemKind::Terminal, Placement::RightOf(a)).unwrap();
        store.raise(1, a);
        store.set_item_cwd(1, b, PathBuf::from("/usr"));
        store.save();
        let before = store.get(1).unwrap().clone();

        let reloaded = store_in(&dir);
        assert_eq!(reloaded.get(1).unwrap(), &before, "windows, order, sessions, folders");
    }

    #[test]
    fn adding_a_folder_with_a_project_restores_it() {
        let (mut store, dir) = canvas_store("reopen");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 1., y: 2. }).unwrap();
        let session = store.get(1).unwrap().item(a).unwrap().session.clone();
        store.delete(1);
        assert!(store.projects().is_empty());

        let folder = dir.join("project");
        let existing = existing_project(&folder).unwrap();
        assert_eq!((existing.name.as_str(), existing.windows), ("P", 4));
        store.create("Renamed".into(), IconName::Zap, ProjectColor::Rose, Some(folder), None);
        let project = store.projects().last().unwrap();
        assert_eq!(project.name, "Renamed");
        assert_eq!(project.items.len(), 4, "the explorer, lino, git and the terminal");
        assert_eq!(project.items[3].session, session, "same terminal session");
    }

    #[test]
    fn copied_folders_get_fresh_window_ids() {
        let (mut store, dir) = canvas_store("dup-ids");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 0., y: 0. }).unwrap();
        store.save();
        // The same project folder, copied: its window ids collide.
        let copy = dir.join("copy");
        fs::create_dir_all(copy.join(PROJECT_DIR)).unwrap();
        for file in [PROJECT_FILE, STATE_FILE] {
            let from = dir.join("project").join(PROJECT_DIR).join(file);
            fs::copy(from, copy.join(PROJECT_DIR).join(file)).unwrap();
        }
        store.create("Copy".into(), IconName::Box, ProjectColor::Blue, Some(copy), None);
        let copied = store.projects().last().unwrap();
        let copied_terminal = copied.items.iter().find(|i| i.kind == ItemKind::Terminal).unwrap();
        assert_ne!(copied_terminal.id, a);
        assert_eq!(copied.windows[0].item, copied_terminal.id, "frame follows the new id");
    }

    #[test]
    fn registry_only_projects_from_before_folders_migrate() {
        // The format before folders: everything inline in the registry.
        let dir = temp_dir("migrate");
        let folder = dir.join("site");
        fs::create_dir_all(&folder).unwrap();
        let registry = serde_json::json!({
            "version": 1, "next_id": 3,
            "projects": [
                {"id": 1, "name": "Old", "icon": "box", "color": "rose", "items": [], "columns": []},
                {"id": 2, "name": "Site", "icon": "globe", "color": "blue", "folder": folder,
                 "items": [{"id": 9, "kind": "terminal", "title": "Terminal 1"}],
                 "windows": [{"item": 9, "x": 0, "y": 0, "width": 500, "height": 300}]}
            ]
        });
        fs::write(dir.join("projects.json"), registry.to_string()).unwrap();

        let store = store_in(&dir);
        assert!(store.get(1).unwrap().folder.is_none(), "folderless stays in the registry");
        // The folder project was written to its folder on load...
        assert!(folder.join(PROJECT_DIR).join(PROJECT_FILE).exists());
        let site = store.get(2).unwrap();
        assert_eq!(site.items[0].kind, ItemKind::Explorer, "added on migration");
        assert_eq!(site.items[1].kind, ItemKind::Lino, "added on migration");
        assert_eq!(site.items[2].kind, ItemKind::Git, "added on migration");
        assert_eq!(site.items[3].color, ProjectColor::Gray);
        assert!(!site.items[3].session.is_empty());
        // ...and loads from there next time.
        let again = store_in(&dir);
        assert_eq!(again.get(2).unwrap(), site);
        assert!(again.next_id > 10, "next id is past every existing id");
    }

    #[test]
    fn missing_registry_is_empty_and_invalid_is_kept() {
        let dir = temp_dir("invalid");
        assert!(store_in(&dir).projects().is_empty());
        fs::write(dir.join("projects.json"), "{ not json").unwrap();
        assert!(store_in(&dir).projects().is_empty());
        assert_eq!(fs::read_to_string(dir.join("projects.json.bak")).unwrap(), "{ not json");
    }

    #[test]
    fn a_broken_state_file_means_a_fresh_start_not_a_lost_project() {
        let (mut store, dir) = canvas_store("bad-state");
        store.add_item(1, ItemKind::Terminal, Placement::At { x: 0., y: 0. }).unwrap();
        store.save();
        fs::write(dir.join("project").join(PROJECT_DIR).join(STATE_FILE), "garbage").unwrap();
        let reloaded = store_in(&dir);
        let project = reloaded.get(1).unwrap();
        assert_eq!(project.windows.len(), 1, "the layout survives");
        assert_eq!(project.viewport, Viewport::default());
    }

    #[test]
    fn windows_open_at_the_default_size_without_overlapping() {
        let (mut store, _) = canvas_store("place");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 10., y: 20. }).unwrap();
        assert_eq!(
            (frame(&store, a).x, frame(&store, a).y, frame(&store, a).width),
            (10., 20., DEFAULT_WINDOW_WIDTH)
        );
        let b = store.add_item(1, ItemKind::Terminal, Placement::RightOf(a)).unwrap();
        assert_eq!(frame(&store, b).x, 10. + DEFAULT_WINDOW_WIDTH + WINDOW_GAP);
        // Right of `a` is taken by `b`, so `c` slides past it.
        let c = store.add_item(1, ItemKind::Terminal, Placement::RightOf(a)).unwrap();
        assert_eq!(frame(&store, c).x, frame(&store, b).x + DEFAULT_WINDOW_WIDTH + WINDOW_GAP);
    }

    #[test]
    fn opening_below_keeps_the_left_edge_and_width() {
        let (mut store, _) = canvas_store("below");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 10., y: 20. }).unwrap();
        store.set_frame(1, a, 10., 20., 900., 300.);
        let b = store.add_item(1, ItemKind::Terminal, Placement::Below(a)).unwrap();
        let b = frame(&store, b);
        assert_eq!((b.x, b.y, b.width), (10., 20. + 300. + WINDOW_GAP, 900.));
        let c = store.add_item(1, ItemKind::Terminal, Placement::Below(a)).unwrap();
        assert_eq!(frame(&store, c).y, b.y + b.height + WINDOW_GAP);
    }

    #[test]
    fn closing_keeps_the_item_and_its_session() {
        let (mut store, _) = canvas_store("close");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 0., y: 0. }).unwrap();
        let session = store.get(1).unwrap().item(a).unwrap().session.clone();
        store.close_item(1, a);
        let project = store.get(1).unwrap();
        assert!(!project.is_open(a) && project.item(a).unwrap().session == session);
        store.open_item(1, a, Placement::At { x: 5., y: 5. });
        assert_eq!((frame(&store, a).x, frame(&store, a).y), (5., 5.));
        store.remove_item(1, a);
        assert!(terminals(&store).is_empty());
    }

    #[test]
    fn frames_keep_a_minimum_size() {
        let (mut store, _) = canvas_store("frames");
        let a = store.add_item(1, ItemKind::Terminal, Placement::At { x: 0., y: 0. }).unwrap();
        store.set_frame(1, a, -50., 70., 10., 10.);
        assert_eq!(
            (frame(&store, a).width, frame(&store, a).height),
            (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)
        );
        store.set_viewport(1, Viewport { x: 1., y: 2., zoom: 99. });
        assert_eq!(store.get(1).unwrap().viewport.zoom, MAX_ZOOM);
    }

    #[test]
    fn deleted_ids_are_not_reused() {
        let dir = temp_dir("delete");
        let mut store = store_in(&dir);
        store.create("One".into(), IconName::Box, ProjectColor::Blue, None, None);
        store.create("Two".into(), IconName::Box, ProjectColor::Blue, None, None);
        store.delete(2);
        let mut reloaded = store_in(&dir);
        reloaded.create("Three".into(), IconName::Box, ProjectColor::Blue, None, None);
        assert_eq!(reloaded.projects().last().unwrap().id, 3);
    }
}
