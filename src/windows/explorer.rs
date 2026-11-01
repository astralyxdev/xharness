//! The Explorer window: a folder as a list, like Finder's list view.
//! Folders expand in place, columns sort, and a filter searches the whole
//! folder. A project can have several, each showing its own folder.
//!
//! - Click a row to select it; double-click (or Enter) opens it: folders
//!   expand, files open in an editor. ↑/↓ move, → opens a folder, ← closes
//!   it (or goes up to its parent). ⌘F jumps to the filter.
//! - Any folder: `..` (or ⌘↑) goes up, the path bar's folders go to them,
//!   and ⇧⌘G (or clicking the bar) takes a path: `~/dev`, `../lib`, `/tmp`.
//! - Drag rows onto a folder to move them there (⌥ copies), onto a terminal
//!   to type their paths, or onto the canvas to open them. Files dropped
//!   from Finder are copied in.
//! - The filter matches names anywhere in the project. Plain text matches
//!   anywhere in a name, ignoring case; `*` and `?` are wildcards (`*.rs`);
//!   text with a `/` matches the path from the project folder (`src/*.rs`).
//! - View: folders first, hidden files, files ignored by git.
//! - Columns: Kind, Size and Modified can be shown or hidden; click a
//!   header to sort by it, again to reverse.
//! - Right-click a file: Open, Open with default app, Reveal in Finder,
//!   Copy path, Open terminal here; a folder can also open in a new
//!   explorer.
//!
//! Listings come from the `ignore` crate (ripgrep's), so .gitignore rules
//! apply the way git applies them. Open folders are read again every few
//! seconds, so changes on disk show up. Everything is sized in rems, so the
//! canvas zooms it with the window.

use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui::{
    App, ClickEvent, Context, ElementId, Entity, EventEmitter, ExternalPaths, FocusHandle, Focusable,
    FontWeight, KeyBinding, MouseButton, MouseDownEvent, Pixels, Point, ScrollStrategy, SharedString,
    Subscription, Task, Transformation, UniformListScrollHandle, Window, actions, div, prelude::*, px,
    radians, rems, uniform_list,
};

use crate::ui::actions::{Cancel, Confirm, SelectNext, SelectPrevious};
use crate::ui::assets::IconName;
use crate::ui::context_menu::MenuEntry;
use crate::services::files::{self, Entry, ListOptions, SEARCH_LIMIT, list_dir, transfer};
use crate::services::projects::{ExplorerColumn, ExplorerSettings, SortKey};
use crate::services::server::HostAddress;
use crate::ui::text_input::TextInput;
use crate::ui::theme::Theme;
use crate::ui;

actions!(explorer, [Expand, Collapse, FocusFilter, GoUp, GoToFolder]);

const KEY_CONTEXT: &str = "Explorer";
/// How often open folders are read again.
const REFRESH_EVERY: Duration = Duration::from_secs(3);
/// The filter searches once typing pauses this long.
const SEARCH_DELAY: Duration = Duration::from_millis(150);

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("right", Expand, context),
        KeyBinding::new("left", Collapse, context),
        KeyBinding::new("enter", Confirm, context),
        KeyBinding::new("escape", Cancel, context),
        KeyBinding::new("secondary-f", FocusFilter, context),
        // As in Finder.
        KeyBinding::new("secondary-up", GoUp, context),
        KeyBinding::new("secondary-shift-g", GoToFolder, context),
    ]);
}

/// Files being dragged out of an explorer. Explorers, terminals and the
/// canvas take them, as they take files dragged in from Finder.
#[derive(Clone)]
pub struct DraggedFiles(pub Vec<PathBuf>);

/// What follows the pointer while dragging files.
pub struct DragLabel(SharedString);

impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        div()
            .flex()
            .items_center()
            .gap_1p5()
            .px_2()
            .py_1()
            .rounded(px(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .text_size(px(12.))
            .child(ui::icon(IconName::File).size_3p5().text_color(theme.muted_foreground))
            .child(self.0.clone())
    }
}

/// `text` as a folder: `~` is home, and relative paths start at `base`.
/// `..` and `.` are resolved.
pub fn resolve_path(text: &str, base: &Path) -> PathBuf {
    let text = text.trim();
    let home = std::env::home_dir().unwrap_or_default();
    let joined = if text == "~" {
        home
    } else if let Some(rest) = text.strip_prefix("~/") {
        home.join(rest)
    } else {
        base.join(text)
    };
    let mut path = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                path.pop();
            }
            Component::CurDir => {}
            other => path.push(other),
        }
    }
    path
}

/// The folders from the top (home, or `/`) down to `root`, for the path bar.
fn crumbs(root: &Path) -> Vec<(String, PathBuf)> {
    let home = std::env::home_dir();
    let (mut crumbs, mut at, rest) = match home.as_ref().and_then(|home| Some((home, root.strip_prefix(home).ok()?))) {
        Some((home, rest)) => (vec![("~".to_string(), home.clone())], home.clone(), rest.to_path_buf()),
        None => (
            vec![("/".to_string(), PathBuf::from("/"))],
            PathBuf::from("/"),
            root.strip_prefix("/").unwrap_or(root).to_path_buf(),
        ),
    };
    for part in rest.components() {
        at.push(part);
        crumbs.push((part.as_os_str().to_string_lossy().into_owned(), at.clone()));
    }
    crumbs
}

pub enum ExplorerEvent {
    /// Sorting, columns, view options or open folders changed (to save).
    SettingsChanged(ExplorerSettings),
    /// "Open terminal here" on this folder.
    OpenTerminal(PathBuf),
    /// Open this file in an editor.
    OpenFile(PathBuf),
    /// Open this folder in a new explorer.
    OpenExplorer(PathBuf),
    /// Show this menu at `position` (window coordinates).
    Menu {
        entries: Vec<MenuEntry>,
        position: Point<Pixels>,
    },
}

// Sorting and describing ---------------------------------------------------------

/// Names compared the way people read them: `file2` before `file10`, and
/// ignoring case.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |chars: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(c) = chars.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        chars.next();
                    }
                    digits
                };
                let (x, y) = (take(&mut a), take(&mut b));
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                let ordering = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(x), Some(y)) => {
                let ordering = x.to_lowercase().cmp(y.to_lowercase());
                if ordering != Ordering::Equal {
                    return ordering;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn compare(a: &Entry, b: &Entry, settings: &ExplorerSettings) -> Ordering {
    if settings.folders_first && a.is_dir != b.is_dir {
        return b.is_dir.cmp(&a.is_dir);
    }
    let by_name = || natural_cmp(&a.name, &b.name);
    let ordering = match settings.sort {
        SortKey::Name => by_name(),
        SortKey::Kind => kind(a).cmp(&kind(b)).then_with(by_name),
        SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
        SortKey::Modified => a.modified.cmp(&b.modified).then_with(by_name),
    };
    if settings.descending {
        ordering.reverse()
    } else {
        ordering
    }
}

fn extension(entry: &Entry) -> Option<String> {
    Path::new(&entry.name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
}

/// What the Kind column says.
fn kind(entry: &Entry) -> Cow<'static, str> {
    if entry.is_dir {
        return "Folder".into();
    }
    let Some(extension) = extension(entry) else {
        return "Document".into();
    };
    let known = match extension.as_str() {
        "rs" => "Rust",
        "toml" => "TOML",
        "json" | "jsonc" => "JSON",
        "md" | "markdown" | "mdx" => "Markdown",
        "js" | "mjs" | "cjs" => "JavaScript",
        "jsx" => "JSX",
        "ts" | "mts" | "cts" => "TypeScript",
        "tsx" => "TSX",
        "html" | "htm" => "HTML",
        "css" | "scss" | "sass" => "Stylesheet",
        "py" => "Python",
        "go" => "Go",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cpp" | "cc" | "hpp" => "C++",
        "java" => "Java",
        "kt" => "Kotlin",
        "rb" => "Ruby",
        "sh" | "zsh" | "bash" | "fish" => "Shell script",
        "yml" | "yaml" => "YAML",
        "xml" => "XML",
        "sql" => "SQL",
        "lock" => "Lockfile",
        "txt" | "log" => "Text",
        "csv" | "tsv" => "Spreadsheet",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "heic" | "bmp" => "Image",
        "mp4" | "mov" | "webm" => "Video",
        "mp3" | "wav" | "flac" | "m4a" => "Audio",
        "pdf" => "PDF",
        "zip" | "tar" | "gz" | "tgz" | "xz" | "7z" | "rar" => "Archive",
        _ => return format!("{} file", extension.to_uppercase()).into(),
    };
    known.into()
}

fn icon_for(entry: &Entry, expanded: bool) -> IconName {
    if entry.is_dir {
        return if expanded { IconName::FolderOpen } else { IconName::Folder };
    }
    match kind(entry).as_ref() {
        "Image" => IconName::FileImage,
        "Markdown" | "Text" | "Document" | "PDF" | "Spreadsheet" => IconName::FileText,
        "Lockfile" | "Archive" | "Video" | "Audio" => IconName::File,
        name if name.ends_with(" file") => IconName::File,
        _ => IconName::FileCode,
    }
}

/// Sizes as Finder shows them (decimal units).
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000. && unit < UNITS.len() - 1 {
        value /= 1000.;
        unit += 1;
    }
    if value < 10. {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// The local date and time of `secs` since the epoch.
fn local_time(secs: i64) -> Option<libc::tm> {
    let time = secs as libc::time_t;
    // SAFETY: localtime_r/localtime_s write into the tm we pass and nothing
    // else.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    #[cfg(windows)]
    let ok = unsafe { libc::localtime_s(&mut tm, &time) == 0 };
    ok.then_some(tm)
}

/// "Today, 14:32", "Yesterday, 09:05", or "12 Oct 2026".
fn format_modified(time: SystemTime, now: SystemTime) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = |time: SystemTime| match time.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    };
    let (Some(local), Some(today)) = (local_time(secs(time)), local_time(secs(now))) else {
        return String::new();
    };
    let yesterday = local_time(secs(now) - 86_400);
    let same_day = |a: &libc::tm, b: &libc::tm| (a.tm_year, a.tm_yday) == (b.tm_year, b.tm_yday);
    let clock = format!("{:02}:{:02}", local.tm_hour, local.tm_min);
    if same_day(&local, &today) {
        format!("Today, {clock}")
    } else if yesterday.is_some_and(|y| same_day(&local, &y)) {
        format!("Yesterday, {clock}")
    } else {
        format!(
            "{} {} {}",
            local.tm_mday,
            MONTHS[local.tm_mon.clamp(0, 11) as usize],
            1900 + local.tm_year
        )
    }
}

pub(crate) fn open_with_default_app(path: &Path, reveal: bool) {
    let mut command = if cfg!(target_os = "macos") {
        let mut open = std::process::Command::new("open");
        if reveal {
            open.arg("-R");
        }
        open.arg(path);
        open
    } else if cfg!(windows) {
        // Explorer opens files with their app, and /select shows one.
        let mut explorer = std::process::Command::new("explorer");
        if reveal {
            explorer.arg(format!("/select,{}", path.display()));
        } else {
            explorer.arg(path);
        }
        explorer
    } else {
        let mut open = std::process::Command::new("xdg-open");
        open.arg(if reveal { path.parent().unwrap_or(path) } else { path });
        open
    };
    command.spawn().ok();
}

// The view -----------------------------------------------------------------------

struct Row {
    entry: Entry,
    depth: usize,
    expanded: bool,
    /// The `..` row at the top: the folder above.
    up: bool,
}

struct Search {
    query: String,
    entries: Vec<Entry>,
    truncated: bool,
    done: bool,
}

pub struct ExplorerView {
    root: PathBuf,
    /// The project's folder, where an explorer with no folder of its own
    /// starts.
    project_root: Option<PathBuf>,
    /// The server the folder is on (`None`: this machine).
    server: Option<HostAddress>,
    settings: ExplorerSettings,
    /// Folder contents read so far, by folder.
    listings: HashMap<PathBuf, Vec<Entry>>,
    loading: HashSet<PathBuf>,
    expanded: HashSet<PathBuf>,
    /// What's on screen, top to bottom.
    rows: Vec<Row>,
    selected: Option<PathBuf>,
    filter: Entity<TextInput>,
    /// The path bar, while typing a folder into it.
    path_input: Entity<TextInput>,
    editing_path: bool,
    /// Why the last move, copy or "go to" failed.
    error: Option<String>,
    search: Option<Search>,
    search_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    _refresh: Task<()>,
    _filter_subscription: Subscription,
}

impl EventEmitter<ExplorerEvent> for ExplorerView {}

impl Focusable for ExplorerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ExplorerView {
    pub fn new(
        root: PathBuf,
        project_root: Option<PathBuf>,
        settings: ExplorerSettings,
        server: Option<HostAddress>,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| TextInput::new("Filter  ·  *.rs  ·  src/", cx).inline());
        let path_input = cx.new(|cx| TextInput::new("A folder: ~/dev, ../lib, /tmp", cx).inline());
        let filter_subscription = cx.observe(&filter, |this, _, cx| this.filter_changed(cx));
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_EVERY).await;
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        });
        let expanded = settings.expanded.iter().map(|relative| root.join(relative)).collect();
        let mut this = Self {
            root,
            project_root,
            server,
            settings,
            listings: HashMap::new(),
            loading: HashSet::new(),
            expanded,
            rows: Vec::new(),
            selected: None,
            filter,
            path_input,
            editing_path: false,
            error: None,
            search: None,
            search_task: None,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _refresh: refresh,
            _filter_subscription: filter_subscription,
        };
        this.refresh(cx);
        this
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// The folder shown.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn settings(&self) -> &ExplorerSettings {
        &self.settings
    }

    /// Shows another folder.
    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.editing_path = false;
        self.error = None;
        if root == self.root {
            cx.notify();
            return;
        }
        self.root = root.clone();
        self.listings.clear();
        self.loading.clear();
        self.expanded.clear();
        self.selected = None;
        if !self.filter.read(cx).text().is_empty() {
            self.filter.update(cx, |filter, cx| filter.set_text("", cx));
        }
        self.search = None;
        self.search_task = None;
        self.settings.root = (Some(&root) != self.project_root.as_ref()).then_some(root);
        self.settings.expanded.clear();
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.rebuild();
        self.refresh(cx);
        cx.emit(ExplorerEvent::SettingsChanged(self.settings.clone()));
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else {
            return;
        };
        let came_from = self.root.clone();
        self.set_root(parent, cx);
        // Where you came from stays picked, as in Finder.
        self.selected = Some(came_from);
    }

    fn start_editing_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_path = true;
        let mut text = ui::display_path(&self.root);
        if !text.ends_with('/') {
            text.push('/');
        }
        self.path_input.update(cx, |input, cx| input.set_text(text, cx));
        window.focus(&self.path_input.focus_handle(cx));
        cx.notify();
    }

    /// Goes to the path typed in the path bar (a file opens in an editor).
    fn go_to_typed_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).text().to_string();
        let path = resolve_path(&text, &self.root);
        if path.is_dir() {
            self.set_root(path, cx);
        } else if path.is_file() {
            if let Some(parent) = path.parent() {
                self.set_root(parent.to_path_buf(), cx);
            }
            self.selected = Some(path.clone());
            cx.emit(ExplorerEvent::OpenFile(path));
        } else {
            self.error = Some(format!("{} isn't there.", text.trim()));
            cx.notify();
            return;
        }
        window.focus(&self.focus_handle);
    }

    /// Files dropped from Finder: copied in (on this Mac only, for now).
    fn drop_external(&mut self, sources: Vec<PathBuf>, into: PathBuf, cx: &mut Context<Self>) {
        if self.server.is_some() {
            self.error = Some("Files from this Mac can't be copied to a remote server yet.".to_string());
            cx.notify();
            return;
        }
        self.drop_files(sources, into, true, cx);
    }

    /// Files dropped on `into` (a folder): moved there, or copied.
    fn drop_files(&mut self, sources: Vec<PathBuf>, into: PathBuf, copy: bool, cx: &mut Context<Self>) {
        let server = self.server.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::services::rpc::on(server.as_ref(), || transfer(&sources, &into, copy)) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(placed) => {
                        this.error = None;
                        if let Some(first) = placed.first() {
                            this.selected = Some(first.clone());
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                this.listings.clear();
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn options(&self) -> ListOptions {
        ListOptions {
            show_hidden: self.settings.show_hidden,
            show_ignored: self.settings.show_ignored,
        }
    }

    // Reading the disk ---------------------------------------------------------

    /// Reads a folder in the background, then shows it.
    fn load(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !self.loading.insert(dir.clone()) {
            return;
        }
        let options = self.options();
        let server = self.server.clone();
        cx.spawn(async move |this, cx| {
            let listing = {
                let dir = dir.clone();
                cx.background_executor()
                    .spawn(async move { crate::services::rpc::on(server.as_ref(), || list_dir(&dir, options)) })
                    .await
            };
            this.update(cx, |this, cx| {
                this.loading.remove(&dir);
                // Options may have changed while we read; that load wins.
                if this.options() == options && this.listings.get(&dir) != Some(&listing) {
                    this.listings.insert(dir, listing);
                    this.rebuild();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reads the project folder and every open folder again.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(self.root.clone(), cx);
        let expanded: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        for dir in expanded {
            self.load(dir, cx);
        }
    }

    fn filter_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.filter.read(cx).text().trim().to_string();
        // The field also notifies for caret moves.
        if self.search.as_ref().map(|s| s.query.as_str()).unwrap_or("") == query {
            return;
        }
        self.run_search(query, cx);
    }

    fn run_search(&mut self, query: String, cx: &mut Context<Self>) {
        if query.trim().is_empty() {
            self.search = None;
            self.search_task = None;
            self.rebuild();
            cx.notify();
            return;
        };
        // Keep showing the last results until the new ones are in.
        match &mut self.search {
            Some(search) => {
                search.query = query.clone();
                search.done = false;
            }
            None => {
                self.search = Some(Search {
                    query: query.clone(),
                    entries: Vec::new(),
                    truncated: false,
                    done: false,
                })
            }
        }
        let (root, options) = (self.root.clone(), self.options());
        let query_for_search = query.clone();
        let server = self.server.clone();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DELAY).await;
            let (entries, truncated) = cx
                .background_executor()
                .spawn(async move { crate::services::rpc::on(server.as_ref(), || files::search(&root, &query_for_search, options)) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(search) = &mut this.search
                    && search.query == query
                {
                    search.entries = entries;
                    search.truncated = truncated;
                    search.done = true;
                }
                this.rebuild();
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // Settings -----------------------------------------------------------------

    /// After a settings change: re-sort, re-read if what's listed changed,
    /// and tell the canvas (which saves it).
    fn settings_changed(&mut self, relist: bool, cx: &mut Context<Self>) {
        if relist {
            self.listings.clear();
            self.refresh(cx);
            if let Some(query) = self.search.as_ref().map(|s| s.query.clone()) {
                self.search = None;
                self.run_search(query, cx);
            }
        }
        self.rebuild();
        cx.emit(ExplorerEvent::SettingsChanged(self.settings.clone()));
        cx.notify();
    }

    fn sort_by(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if self.settings.sort == key {
            self.settings.descending = !self.settings.descending;
        } else {
            self.settings.sort = key;
            self.settings.descending = false;
        }
        self.settings_changed(false, cx);
    }

    fn toggle_column(&mut self, column: ExplorerColumn, cx: &mut Context<Self>) {
        let mut columns = self.settings.columns.clone();
        if columns.contains(&column) {
            columns.retain(|c| *c != column);
        } else {
            columns.push(column);
        }
        // Keep the usual order whatever order they were turned on in.
        self.settings.columns = ExplorerColumn::ALL
            .into_iter()
            .filter(|c| columns.contains(c))
            .collect();
        self.settings_changed(false, cx);
    }

    fn toggle_folder(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
            self.load(path.to_path_buf(), cx);
        }
        let mut expanded: Vec<PathBuf> = self
            .expanded
            .iter()
            .filter_map(|path| path.strip_prefix(&self.root).ok().map(Path::to_path_buf))
            .collect();
        expanded.sort();
        self.settings.expanded = expanded;
        self.settings_changed(false, cx);
    }

    // Rows -----------------------------------------------------------------------

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        if self.search.is_none()
            && let Some(parent) = self.root.parent()
        {
            rows.push(Row {
                entry: Entry {
                    path: parent.to_path_buf(),
                    name: "..".into(),
                    is_dir: true,
                    size: 0,
                    modified: None,
                },
                depth: 0,
                expanded: false,
                up: true,
            });
        }
        if let Some(search) = &self.search {
            let mut entries = search.entries.clone();
            entries.sort_by(|a, b| compare(a, b, &self.settings));
            rows.extend(entries.into_iter().map(|entry| Row {
                entry,
                depth: 0,
                expanded: false,
                up: false,
            }));
        } else {
            self.push_rows(&self.root, 0, &mut rows);
        }
        self.rows = rows;
    }

    fn push_rows(&self, dir: &Path, depth: usize, rows: &mut Vec<Row>) {
        let Some(listing) = self.listings.get(dir) else {
            return;
        };
        let mut entries: Vec<&Entry> = listing.iter().collect();
        entries.sort_by(|a, b| compare(a, b, &self.settings));
        for entry in entries {
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            rows.push(Row {
                entry: entry.clone(),
                depth,
                expanded,
                up: false,
            });
            if expanded {
                self.push_rows(&entry.path, depth + 1, rows);
            }
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows.iter().position(|row| &row.entry.path == selected)
    }

    fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(index) {
            self.selected = Some(row.entry.path.clone());
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn open(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let entry = row.entry.clone();
        if row.up {
            self.go_up(cx);
        } else if entry.is_dir && self.search.is_none() {
            self.toggle_folder(&entry.path, cx);
        } else if entry.is_dir {
            // A folder in search results: go there.
            self.set_root(entry.path, cx);
        } else {
            cx.emit(ExplorerEvent::OpenFile(entry.path));
        }
    }

    // Keyboard -------------------------------------------------------------------

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.selected_index().map_or(0, |ix| ix + 1);
        self.select_index(next.min(self.rows.len().saturating_sub(1)), cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let previous = self.selected_index().map_or(0, |ix| ix.saturating_sub(1));
        self.select_index(previous, cx);
    }

    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else {
            return;
        };
        let row = &self.rows[ix];
        if !row.entry.is_dir || self.search.is_some() {
            return;
        }
        if row.expanded {
            self.select_index(ix + 1, cx);
        } else {
            let path = row.entry.path.clone();
            self.toggle_folder(&path, cx);
        }
    }

    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else {
            return;
        };
        let row = &self.rows[ix];
        if row.expanded {
            let path = row.entry.path.clone();
            self.toggle_folder(&path, cx);
        } else if let Some(parent) = row.entry.path.parent()
            && let Some(parent_ix) = self.rows.iter().position(|r| r.entry.path == parent)
        {
            self.select_index(parent_ix, cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing_path {
            self.go_to_typed_path(window, cx);
            return;
        }
        if let Some(ix) = self.selected_index() {
            self.open(ix, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing_path {
            self.editing_path = false;
            self.error = None;
            window.focus(&self.focus_handle);
            cx.notify();
            return;
        }
        if self.filter.read(cx).text().is_empty() {
            cx.propagate();
            return;
        }
        self.filter.update(cx, |filter, cx| filter.set_text("", cx));
        window.focus(&self.focus_handle);
    }

    fn focus_filter(&mut self, _: &FocusFilter, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.filter.focus_handle(cx));
    }

    // Menus ----------------------------------------------------------------------

    fn view_menu(&self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let toggle = |change: fn(&mut ExplorerSettings), relist: bool| {
            let this = this.clone();
            move |_: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| {
                    change(&mut this.settings);
                    this.settings_changed(relist, cx);
                })
                .ok();
            }
        };
        let s = &self.settings;
        let refresh = {
            let this = this.clone();
            move |_: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| this.refresh(cx)).ok();
            }
        };
        let entries = vec![
            MenuEntry::check("Folders first", s.folders_first, toggle(|s| s.folders_first = !s.folders_first, false)),
            MenuEntry::check("Show hidden files", s.show_hidden, toggle(|s| s.show_hidden = !s.show_hidden, true)),
            MenuEntry::check(
                "Show files ignored by git",
                s.show_ignored,
                toggle(|s| s.show_ignored = !s.show_ignored, true),
            ),
            MenuEntry::Separator,
            MenuEntry::item(IconName::RefreshCw, "Refresh", refresh),
        ];
        cx.emit(ExplorerEvent::Menu { entries, position });
    }

    fn columns_menu(&self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let entries = ExplorerColumn::ALL
            .into_iter()
            .map(|column| {
                let this = this.clone();
                MenuEntry::check(column.label(), self.settings.columns.contains(&column), move |_, cx| {
                    this.update(cx, |this, cx| this.toggle_column(column, cx)).ok();
                })
            })
            .collect();
        cx.emit(ExplorerEvent::Menu { entries, position });
    }

    fn row_menu(&self, entry: &Entry, position: Point<Pixels>, cx: &mut Context<Self>) {
        let path = entry.path.clone();
        let relative = path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf();
        let folder = if entry.is_dir {
            path.clone()
        } else {
            path.parent().unwrap_or(&self.root).to_path_buf()
        };
        let this = cx.entity().downgrade();
        let copy = |text: String| move |_: &mut Window, cx: &mut App| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
        };
        let emit = |event: ExplorerEvent| {
            let this = this.clone();
            let event = std::cell::RefCell::new(Some(event));
            move |_: &mut Window, cx: &mut App| {
                if let Some(event) = event.borrow_mut().take() {
                    this.update(cx, |_, cx| cx.emit(event)).ok();
                }
            }
        };
        let mut entries = if entry.is_dir {
            vec![
                MenuEntry::item(IconName::FolderOpen, "Open here", {
                    let (this, path) = (this.clone(), path.clone());
                    move |_, cx| {
                        this.update(cx, |this, cx| this.set_root(path.clone(), cx)).ok();
                    }
                }),
                MenuEntry::item(IconName::FolderTree, "Open in new explorer", emit(ExplorerEvent::OpenExplorer(path.clone()))),
            ]
        } else {
            vec![
                MenuEntry::item(IconName::FileCode, "Open", emit(ExplorerEvent::OpenFile(path.clone()))),
            ]
        };
        if self.server.is_none() {
            if !entry.is_dir {
                entries.push(MenuEntry::item(IconName::ExternalLink, "Open with default app", {
                    let path = path.clone();
                    move |_, _| open_with_default_app(&path, false)
                }));
            }
            entries.push(MenuEntry::item(IconName::FolderOpen, "Reveal in Finder", {
                let path = path.clone();
                move |_, _| open_with_default_app(&path, true)
            }));
        }
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::item(IconName::Copy, "Copy path", copy(path.display().to_string())),
            MenuEntry::item(IconName::Copy, "Copy relative path", copy(relative.display().to_string())),
            MenuEntry::Separator,
            MenuEntry::item(IconName::SquareTerminal, "Open terminal here", move |_, cx| {
                let folder = folder.clone();
                this.update(cx, |_, cx| cx.emit(ExplorerEvent::OpenTerminal(folder))).ok();
            }),
        ]);
        cx.emit(ExplorerEvent::Menu { entries, position });
    }

    // Rendering ------------------------------------------------------------------

    fn render_toolbar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let filtering = !self.filter.read(cx).text().is_empty();
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .h_10()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                ui::small_icon_button("explorer-up", IconName::ArrowUpFromLine, theme)
                    .when(self.root.parent().is_none(), |button| button.opacity(0.4))
                    .on_click(cx.listener(|this, _, _, cx| this.go_up(cx))),
            )
            .child(
                // The filter: border bg-background h-7, like a small Field.
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1p5()
                    .h_7()
                    .pl_2()
                    .pr_1()
                    .rounded(rems(10. / 16.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .text_color(theme.foreground)
                    .child(ui::icon(IconName::Search).size_3p5().text_color(theme.muted_foreground))
                    .child(div().flex_1().min_w_0().child(self.filter.clone()))
                    .when(filtering, |field| {
                        field.child(
                            ui::small_icon_button("clear-filter", IconName::X, theme).on_click(cx.listener(
                                |this, _, window, cx| this.cancel(&Cancel, window, cx),
                            )),
                        )
                    }),
            )
            .child(
                ui::small_icon_button("explorer-view", IconName::SlidersHorizontal, theme).on_click(
                    cx.listener(|this, event: &ClickEvent, _, cx| this.view_menu(event.position(), cx)),
                ),
            )
            .child(
                ui::small_icon_button("explorer-columns", IconName::ColumnsThree, theme).on_click(
                    cx.listener(|this, event: &ClickEvent, _, cx| this.columns_menu(event.position(), cx)),
                ),
            )
    }

    /// The folder shown, as clickable folders from home (or `/`) down; click
    /// past them, or ⇧⌘G, to type a path instead.
    fn render_path_bar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let bar = div()
            .id("explorer-path")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_0p5()
            .h_7()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .text_size(rems(0.75))
            .line_height(rems(1.))
            .text_color(theme.muted_foreground)
            .overflow_hidden();
        if self.editing_path {
            return bar
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.editing_path = false;
                    cx.notify();
                }))
                .child(ui::icon(IconName::FolderOpen).size_3p5().text_color(theme.tints.blue.base))
                .child(div().flex_1().min_w_0().px_1().text_color(theme.foreground).child(self.path_input.clone()))
                .into_any_element();
        }
        let crumbs = crumbs(&self.root);
        let last = crumbs.len() - 1;
        // Long paths keep their first folder and the last three.
        let hidden = if crumbs.len() > 5 { 1..crumbs.len() - 3 } else { 0..0 };
        let (fg, hover_bg) = (theme.foreground, theme.accent);
        bar.children(crumbs.into_iter().enumerate().filter_map(|(ix, (label, path))| {
            if hidden.contains(&ix) {
                return (ix == hidden.start).then(|| div().px_0p5().child("…").into_any_element());
            }
            let crumb = div()
                .id(ElementId::named_usize("crumb", ix))
                .flex_shrink_0()
                .px_1()
                .rounded(rems(0.25))
                .cursor_pointer()
                .hover(move |style| style.bg(hover_bg).text_color(fg))
                .when(ix == last, |crumb| crumb.text_color(fg).font_weight(FontWeight::MEDIUM))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_root(path.clone(), cx)));
            Some(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_0p5()
                    .when(ix > 0 && !(ix == hidden.end && !hidden.is_empty()), |el| {
                        el.child(ui::icon(IconName::ChevronRight).size_3().text_color(theme.muted_foreground))
                    })
                    .when(ix == hidden.end && !hidden.is_empty(), |el| {
                        el.child(ui::icon(IconName::ChevronRight).size_3().text_color(theme.muted_foreground))
                    })
                    .child(crumb)
                    .into_any_element(),
            )
        }))
        .child(
            div()
                .id("explorer-path-rest")
                .flex_1()
                .h_full()
                .cursor_text()
                .on_click(cx.listener(|this, _, window, cx| this.start_editing_path(window, cx))),
        )
        .into_any_element()
    }

    fn render_header(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let cell = |label: &'static str, key: SortKey| {
            let sorted = self.settings.sort == key;
            let (muted, fg) = (theme.muted_foreground, theme.foreground);
            div()
                .id(label)
                .flex()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .text_color(if sorted { fg } else { muted })
                .hover(move |style| style.text_color(fg))
                .child(label)
                .when(sorted, |cell| {
                    let arrow = if self.settings.descending { IconName::ArrowDown } else { IconName::ArrowUp };
                    cell.child(ui::icon(arrow).size_3())
                })
                .on_click(cx.listener(move |this, _, _, cx| this.sort_by(key, cx)))
        };
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_3()
            .h_7()
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .text_size(rems(0.75))
            .line_height(rems(1.))
            .font_weight(FontWeight::MEDIUM)
            // Right-click the header to pick columns, as in Finder.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _, cx| this.columns_menu(event.position, cx)),
            )
            .child(div().flex_1().min_w_0().pl(rems(2.75)).child(cell("Name", SortKey::Name)))
            .children(self.settings.columns.iter().map(|column| {
                column_cell(*column)
                    .child(cell(column.label(), column.sort_key()))
                    .into_any_element()
            }))
    }

    fn render_row(&self, ix: usize, theme: &Theme, now: SystemTime, cx: &Context<Self>) -> impl IntoElement {
        let row = &self.rows[ix];
        let entry = &row.entry;
        let selected = !row.up && self.selected.as_ref() == Some(&entry.path);
        let searching = self.search.is_some();
        let (muted, hover_bg) = (theme.muted_foreground, theme.accent);
        let icon_color = if entry.is_dir { theme.tints.blue.base } else { muted };
        let path = entry.path.clone();
        let up = row.up;
        // Dropping on a folder puts things in it; on a file, next to it.
        let drop_into = if entry.is_dir {
            path.clone()
        } else {
            path.parent().unwrap_or(&self.root).to_path_buf()
        };
        let drop_bg = theme.tints.blue.base.opacity(0.18);
        let drag_label = SharedString::from(entry.name.clone());
        // In search results, the folder a match is in comes before its name.
        let location = searching
            .then(|| {
                path.parent()
                    .and_then(|parent| parent.strip_prefix(&self.root).ok())
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(|parent| format!("{}/", parent.display()))
            })
            .flatten();

        // The list sizes each row to its content unless told otherwise; a
        // full-width row keeps every row's columns under the header's.
        div().w_full().px_1().child(
        div()
            .id(ElementId::named_usize("row", ix))
            .flex()
            .items_center()
            .gap_3()
            .w_full()
            .h_7()
            .px_2()
            .rounded(rems(6. / 16.))
            .map(|row| {
                if selected {
                    row.bg(theme.tints.blue.soft).text_color(theme.foreground)
                } else {
                    row.hover(move |style| style.bg(hover_bg))
                }
            })
            .on_click(cx.listener({
                let path = path.clone();
                move |this, event: &ClickEvent, window, cx| {
                    this.selected = Some(path.clone());
                    window.focus(&this.focus_handle);
                    if event.click_count() == 2 {
                        this.open(ix, cx);
                    }
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    let Some(row) = this.rows.get(ix).filter(|row| !row.up) else {
                        return;
                    };
                    let entry = row.entry.clone();
                    this.selected = Some(entry.path.clone());
                    this.row_menu(&entry, event.position, cx);
                    cx.notify();
                }),
            )
            .when(!up, |row| {
                row.on_drag(DraggedFiles(vec![path.clone()]), move |_, _, _, cx| {
                    cx.new(|_| DragLabel(drag_label.clone()))
                })
            })
            .when(entry.is_dir, |row| {
                row.drag_over::<DraggedFiles>(move |style, _, _, _| style.bg(drop_bg))
                    .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
            })
            .on_drop(cx.listener({
                let into = drop_into.clone();
                move |this, files: &DraggedFiles, window, cx| {
                    this.drop_files(files.0.clone(), into.clone(), window.modifiers().alt, cx)
                }
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.drop_external(paths.paths().to_vec(), drop_into.clone(), cx)
            }))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1p5()
                    .pl(rems(row.depth as f32))
                    .child(if entry.is_dir && !searching && !up {
                        // Disclosure triangle: opens without selecting.
                        div()
                            .id(ElementId::named_usize("toggle", ix))
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .size_4()
                            .child(
                                ui::icon(IconName::ChevronRight)
                                    .size_3()
                                    .text_color(muted)
                                    .when(row.expanded, |icon| {
                                        icon.with_transformation(Transformation::rotate(radians(
                                            std::f32::consts::FRAC_PI_2,
                                        )))
                                    }),
                            )
                            .on_click(cx.listener({
                                let path = path.clone();
                                move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle_folder(&path, cx);
                                }
                            }))
                            .into_any_element()
                    } else {
                        div().size_4().flex_shrink_0().into_any_element()
                    })
                    .child(
                        ui::icon(if up { IconName::ArrowUpFromLine } else { icon_for(entry, row.expanded) })
                            .text_color(if up { muted } else { icon_color }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .children(location.map(|location| div().flex_shrink_0().text_color(muted).child(location)))
                            .child(div().min_w_0().truncate().child(entry.name.clone())),
                    ),
            )
            .children(self.settings.columns.iter().map(|column| {
                let text: SharedString = match column {
                    _ if up => SharedString::default(),
                    ExplorerColumn::Kind => kind(entry).into_owned().into(),
                    ExplorerColumn::Size if entry.is_dir => "—".into(),
                    ExplorerColumn::Size => format_size(entry.size).into(),
                    ExplorerColumn::Modified => entry
                        .modified
                        .map(|time| format_modified(time, now))
                        .unwrap_or_default()
                        .into(),
                };
                column_cell(*column)
                    .text_color(muted)
                    .child(div().truncate().child(text))
                    .into_any_element()
            })))
    }

    fn status(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let items = self.rows.iter().filter(|row| !row.up).count();
        match &self.search {
            Some(search) if !search.done => "Searching…".to_string(),
            Some(search) if search.truncated => format!("First {SEARCH_LIMIT} matches"),
            Some(_) => count(items, "match", "matches"),
            None => count(items, "item", "items"),
        }
    }
}

/// A Kind/Size/Modified cell, in the header or a row.
fn column_cell(column: ExplorerColumn) -> gpui::Div {
    let cell = div().flex().flex_shrink_0().items_center().overflow_hidden();
    match column {
        ExplorerColumn::Kind => cell.w(rems(7.5)),
        ExplorerColumn::Size => cell.w(rems(5.)).justify_end(),
        ExplorerColumn::Modified => cell.w(rems(8.5)),
    }
}

impl Render for ExplorerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let root_missing = self.server.is_none() && !self.root.is_dir();
        let empty = self.rows.iter().all(|row| row.up) && !self.loading.contains(&self.root);
        let root = self.root.clone();
        let drop_bg = theme.tints.blue.base.opacity(0.08);
        let rows = self.rows.len();

        div()
            .id("explorer")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .text_size(rems(0.8125))
            .line_height(rems(1.25))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::expand))
            .on_action(cx.listener(Self::collapse))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_filter))
            .on_action(cx.listener(|this, _: &GoUp, _, cx| this.go_up(cx)))
            .on_action(cx.listener(|this, _: &GoToFolder, window, cx| this.start_editing_path(window, cx)))
            // The list scrolls; the canvas shouldn't pan along with it.
            .on_scroll_wheel(cx.listener(|this, _, window, cx| {
                if this.contains_focus(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(self.render_toolbar(theme, cx))
            .child(self.render_path_bar(theme, cx))
            .child(self.render_header(theme, cx))
            .child(
                // Dropping anywhere off the rows puts things in the folder shown.
                div()
                    .id("explorer-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .drag_over::<DraggedFiles>(move |style, _, _, _| style.bg(drop_bg))
                    .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
                    .on_drop(cx.listener({
                        let root = root.clone();
                        move |this, files: &DraggedFiles, window, cx| {
                            this.drop_files(files.0.clone(), root.clone(), window.modifiers().alt, cx)
                        }
                    }))
                    .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                        this.drop_external(paths.paths().to_vec(), root.clone(), cx)
                    }))
                    .child(if root_missing || empty {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px_4()
                    .text_center()
                    .text_color(theme.muted_foreground)
                    .child(if root_missing {
                        format!("{} isn't there any more.", ui::display_path(&self.root))
                    } else if self.search.is_some() {
                        "Nothing matches.".to_string()
                    } else {
                        "This folder is empty.".to_string()
                    })
                    .into_any_element()
            } else {
                let now = SystemTime::now();
                uniform_list(
                    "explorer-rows",
                    rows,
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        let theme = Theme::global(cx).clone();
                        range
                            .map(|ix| this.render_row(ix, &theme, now, cx).into_any_element())
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .py_1()
                .track_scroll(self.scroll.clone())
                .into_any_element()
            }),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .h_6()
                    .px_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(rems(0.6875))
                    .text_color(theme.muted_foreground)
                    .child(self.status()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64, modified: u64) -> Entry {
        Entry {
            path: PathBuf::from(name),
            name: name.into(),
            is_dir,
            size,
            modified: Some(UNIX_EPOCH + Duration::from_secs(modified)),
        }
    }

    #[test]
    fn names_sort_naturally() {
        let mut names = vec!["file10", "File2", "file1", "a"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["a", "file1", "File2", "file10"]);
    }

    #[test]
    fn sorting_follows_the_settings() {
        let (src, big, small) = (entry("src", true, 0, 5), entry("big.bin", false, 900, 1), entry("a.txt", false, 10, 9));
        let sorted = |settings: &ExplorerSettings| {
            let mut entries = vec![big.clone(), src.clone(), small.clone()];
            entries.sort_by(|a, b| compare(a, b, settings));
            entries.into_iter().map(|e| e.name).collect::<Vec<_>>()
        };
        let mut settings = ExplorerSettings::default();
        assert_eq!(sorted(&settings), ["src", "a.txt", "big.bin"], "folders first, then by name");
        settings.sort = SortKey::Size;
        settings.descending = true;
        // Descending reverses within each group; folders stay first.
        assert_eq!(sorted(&settings), ["src", "big.bin", "a.txt"]);
        settings.folders_first = false;
        settings.sort = SortKey::Modified;
        settings.descending = false;
        assert_eq!(sorted(&settings), ["big.bin", "src", "a.txt"]);
    }

    #[test]
    fn sizes_and_kinds_read_like_finder() {
        assert_eq!(format_size(12), "12 bytes");
        assert_eq!(format_size(1_500), "1.5 KB");
        assert_eq!(format_size(48_000), "48 KB");
        assert_eq!(format_size(3_200_000_000), "3.2 GB");
        assert_eq!(kind(&entry("main.rs", false, 0, 0)), "Rust");
        assert_eq!(kind(&entry("photo.JPG", false, 0, 0)), "Image");
        assert_eq!(kind(&entry("data.parquet", false, 0, 0)), "PARQUET file");
        assert_eq!(kind(&entry("Makefile", false, 0, 0)), "Document");
        assert_eq!(kind(&entry("src", true, 0, 0)), "Folder");
    }

    #[test]
    fn dates_say_today_and_yesterday() {
        let now = SystemTime::now();
        assert!(format_modified(now - Duration::from_secs(60), now).starts_with("Today, ")
            || format_modified(now - Duration::from_secs(60), now).starts_with("Yesterday, "));
        let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000); // Nov 2023
        assert!(format_modified(old, now).ends_with("2023"));
    }

    #[test]
    fn typed_paths_resolve_like_a_shell() {
        let base = Path::new("/Users/me/dev/app");
        assert_eq!(resolve_path("../lib", base), PathBuf::from("/Users/me/dev/lib"));
        assert_eq!(resolve_path("./src/", base), PathBuf::from("/Users/me/dev/app/src"));
        assert_eq!(resolve_path("/tmp", base), PathBuf::from("/tmp"));
        assert_eq!(resolve_path("../../..", base), PathBuf::from("/Users"));
        let home = std::env::home_dir().unwrap();
        assert_eq!(resolve_path("~/x", base), home.join("x"));
        let trail = crumbs(&home.join("dev/app"));
        assert_eq!(trail.iter().map(|(label, _)| label.as_str()).collect::<Vec<_>>(), ["~", "dev", "app"]);
        assert_eq!(trail[1].1, home.join("dev"));
        assert_eq!(crumbs(Path::new("/tmp/a"))[0].0, "/");
    }

}
