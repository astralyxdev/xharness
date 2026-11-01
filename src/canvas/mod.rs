//! A project's canvas: an infinite surface the project's windows sit on,
//! like a Figma file.
//!
//! - Pan with a two-finger scroll (or the wheel), or by dragging empty
//!   canvas.
//! - Zoom with ⌘/ctrl + scroll around the pointer, ⌘= / ⌘- / ⌘0, ⌘1 to fit
//!   everything, ⌘2 for the active window, or the zoom pill.
//! - Move a window by dragging its header; resize it from any edge or
//!   corner. Clicking a window brings it to the front.
//!
//! Over the window you're working in, scrolling scrolls its terminal; over
//! anything else it pans.
//!
//! Zoom scales windows the way Figma does: the canvas renders each window's
//! contents at a scaled rem size (see [`Scaled`]), so text stays sharp and a
//! terminal keeps its rows and columns at every zoom level.
//!
//! Positions live in the [`ProjectStore`] in canvas units (1 unit = 1px at
//! 100%). The canvas owns the live terminal sessions of the open windows.

use std::{cell::Cell, collections::HashMap, path::PathBuf, rc::Rc, time::Duration};

use gpui::{
    AnyElement, AnyView, App, Bounds, Context, CursorStyle, DragMoveEvent, ElementId, Empty, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, FontWeight, Hsla, KeyBinding,
    MouseButton, MouseDownEvent, Pixels, Point, ScrollWheelEvent, SharedString,
    Subscription, Task, Window, actions, canvas, div, fill, point, prelude::*, px, rems, size,
};

mod geometry;
mod scaled;

pub use geometry::{Edge, fit, resized, zoom_around};
use scaled::Scaled;

use crate::services::projects::{
    DEFAULT_WINDOW_HEIGHT, DEFAULT_WINDOW_WIDTH, ExplorerSettings, Frame, ItemKind, Placement, Project, ProjectItem,
    ProjectStore, Viewport,
};
use crate::services::server::HostAddress;
use crate::ui::assets::IconName;
use crate::ui::context_menu::MenuEntry;
use crate::windows::editor::{EditorEvent, EditorView};
use crate::windows::explorer::{DraggedFiles, ExplorerEvent, ExplorerView};
use crate::windows::git::{GitEvent, GitView};
use crate::windows::lino::LinoView;
use crate::windows::Mods;
use crate::windows::terminal::{SessionSpec, TerminalEvent, TerminalView};
use crate::ui::theme::{Theme, radius};
use crate::ui::{self, Button};

actions!(
    canvas,
    [
        NewTerminal,
        NewTerminalBelow,
        CloseWindow,
        ZoomIn,
        ZoomOut,
        ZoomToActualSize,
        ZoomToFit,
        ZoomToWindow,
    ]
);

const KEY_CONTEXT: &str = "ProjectCanvas";
/// Each zoom step in or out.
const ZOOM_STEP: f32 = 1.25;
/// Live changes (drags, panning) are written this long after they stop.
const SAVE_DELAY: Duration = Duration::from_millis(400);

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-t", NewTerminal, context),
        KeyBinding::new("secondary-shift-d", NewTerminalBelow, context),
        KeyBinding::new("secondary-w", CloseWindow, context),
        KeyBinding::new("secondary-=", ZoomIn, context),
        KeyBinding::new("secondary--", ZoomOut, context),
        KeyBinding::new("secondary-0", ZoomToActualSize, context),
        KeyBinding::new("secondary-1", ZoomToFit, context),
        KeyBinding::new("secondary-2", ZoomToWindow, context),
    ]);
}

/// What the canvas asks of the app shell, which owns menus.
pub enum CanvasEvent {
    /// A window's own menu (its header's ⋯).
    ItemMenu { item: u64, position: Point<Pixels> },
    /// A menu a window built (e.g. the Explorer's View menu).
    Menu {
        item: u64,
        entries: Vec<MenuEntry>,
        position: Point<Pixels>,
    },
    /// Open a terminal in `folder`, next to window `next_to`.
    OpenTerminal { folder: PathBuf, next_to: u64 },
    /// Open `path` in an editor, next to window `next_to`.
    OpenFile { path: PathBuf, next_to: u64 },
    /// Open `path` at a line and UTF-16 character (go to definition).
    OpenFileAt { path: PathBuf, line: usize, character: usize, next_to: u64 },
    /// Open the folder `root` in a new explorer, next to window `next_to`.
    OpenExplorer { root: PathBuf, next_to: u64 },
    /// An editor with unsaved changes is closing: ask first, then call
    /// [`ProjectCanvas::discard_item`].
    ConfirmDiscard { item: u64, name: String },
    /// Ask before something that can't be undone; `action` does it.
    Confirm {
        title: String,
        description: String,
        confirm: String,
        action: Rc<dyn Fn(&mut App)>,
    },
}

/// What an open window shows.
#[derive(Clone)]
enum Content {
    Terminal(Entity<TerminalView>),
    Explorer(Entity<ExplorerView>),
    Lino(Entity<LinoView>),
    Editor(Entity<EditorView>),
    Git(Entity<GitView>),
}

impl Content {
    fn view(&self) -> AnyView {
        match self {
            Content::Terminal(terminal) => terminal.clone().into(),
            Content::Explorer(explorer) => explorer.clone().into(),
            Content::Lino(lino) => lino.clone().into(),
            Content::Editor(editor) => editor.clone().into(),
            Content::Git(git) => git.clone().into(),
        }
    }

    fn focus(&self, window: &mut Window, cx: &App) {
        match self {
            Content::Terminal(terminal) => terminal.read(cx).focus(window, cx),
            Content::Explorer(explorer) => explorer.read(cx).focus(window),
            Content::Lino(lino) => lino.read(cx).focus(window),
            Content::Editor(editor) => editor.read(cx).focus(window),
            Content::Git(git) => git.read(cx).focus(window),
        }
    }

    fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        match self {
            Content::Terminal(terminal) => terminal.read(cx).contains_focus(window, cx),
            Content::Explorer(explorer) => explorer.read(cx).contains_focus(window, cx),
            Content::Lino(lino) => lino.read(cx).contains_focus(window, cx),
            Content::Editor(editor) => editor.read(cx).contains_focus(window, cx),
            Content::Git(git) => git.read(cx).contains_focus(window, cx),
        }
    }

    /// Ends what runs in the window for good (only terminals run anything).
    fn kill(&self, cx: &App) {
        if let Content::Terminal(terminal) = self {
            terminal.read(cx).kill();
        }
    }
}

/// Dragging a window by its header.
pub struct MoveWindow(u64);

/// Dragging one of a window's resize handles.
pub struct ResizeWindow {
    item: u64,
    edge: Edge,
}

/// Dragging empty canvas.
pub struct PanCanvas;

/// Drags are only used for their move events; nothing follows the pointer.
struct NoPreview;

impl Render for NoPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Where a drag started: the pointer, and the frame and viewport then.
#[derive(Clone, Copy)]
struct Gesture {
    mouse: Point<Pixels>,
    frame: Option<Frame>,
    viewport: Viewport,
}

pub struct ProjectCanvas {
    project_id: u64,
    /// The server the project's windows work on (`None`: this machine's).
    server: Option<HostAddress>,
    store: Entity<ProjectStore>,
    /// Live windows for the open items, by item id.
    windows: HashMap<u64, (Content, Vec<Subscription>)>,
    /// The window you're working in: highlighted, scrolls its own content,
    /// and new windows open next to it.
    active: Option<u64>,
    focus_handle: FocusHandle,
    /// The view's bounds in the window, from the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    gesture: Option<Gesture>,
    save_task: Option<Task<()>>,
    _store_subscription: Subscription,
}

impl EventEmitter<CanvasEvent> for ProjectCanvas {}

impl Focusable for ProjectCanvas {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ProjectCanvas {
    pub fn new(project_id: u64, store: Entity<ProjectStore>, cx: &mut Context<Self>) -> Self {
        // Windows opened or closed from elsewhere (the sidebar) start or stop
        // their sessions here.
        let subscription = cx.observe(&store, |this, _, cx| this.sync(cx));
        let server = store.read(cx).get(project_id).and_then(Project::server);
        let mut this = Self {
            project_id,
            server,
            store,
            windows: HashMap::new(),
            active: None,
            focus_handle: cx.focus_handle(),
            bounds: Rc::new(Cell::new(Bounds::default())),
            gesture: None,
            save_task: None,
            _store_subscription: subscription,
        };
        this.sync(cx);
        this.close_uninstalled(cx);
        // Pick up where you left off: the frontmost window.
        this.active = this
            .project(cx)
            .and_then(|p| p.windows.last())
            .map(|frame| frame.item);
        this
    }

    pub fn active_item(&self) -> Option<u64> {
        self.active
    }

    fn project<'a>(&self, cx: &'a App) -> Option<&'a Project> {
        self.store.read(cx).get(self.project_id)
    }

    fn viewport(&self, cx: &App) -> Viewport {
        self.project(cx).map(|p| p.viewport).unwrap_or_default()
    }

    /// The view's size in px (zero before the first paint).
    fn view_size(&self) -> (f32, f32) {
        let size = self.bounds.get().size;
        (f32::from(size.width), f32::from(size.height))
    }

    /// Attaches a view to every open window that has none (resuming its
    /// session in the host, or starting one), and drops the views of
    /// windows that closed.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project(cx) else {
            self.windows.clear();
            return;
        };
        let folder = project.folder.clone();
        let server = self.server.clone();
        // Paths on another machine can't be checked here.
        let here = |path: &PathBuf| server.is_some() || path.is_dir();
        let open: Vec<ProjectItem> = project
            .items
            .iter()
            .filter(|item| project.is_open(item.id))
            .cloned()
            .collect();

        self.windows
            .retain(|id, _| open.iter().any(|item| item.id == *id));
        for item in open {
            let id = item.id;
            if self.windows.contains_key(&id) {
                continue;
            }
            match item.kind {
                ItemKind::Terminal => {
                    let spec = SessionSpec {
                        id: item.session.clone(),
                        // A new shell starts where the last one was.
                        folder: item.cwd.clone().filter(|cwd| here(cwd)).or(folder.clone()),
                        host: server.clone().unwrap_or_default(),
                    };
                    let terminal = cx.new(|cx| TerminalView::new(spec, cx));
                    let subscription = cx.subscribe(&terminal, move |this, terminal, event, cx| {
                        match event {
                            // A shell that exits closes its window, as in
                            // other terminals. The item stays, closed.
                            TerminalEvent::Exited => this.close_item(id, cx),
                            TerminalEvent::TitleChanged => {
                                if let Some(cwd) = terminal.read(cx).cwd() {
                                    let project = this.project_id;
                                    let changed = this
                                        .store
                                        .update(cx, |store, _| store.set_item_cwd(project, id, cwd));
                                    if changed {
                                        this.save_soon(cx);
                                    }
                                }
                                cx.notify();
                            }
                        }
                    });
                    self.windows.insert(id, (Content::Terminal(terminal), vec![subscription]));
                }
                ItemKind::Explorer => {
                    // Its own folder if it has one (and it's still there),
                    // else the project's, else home.
                    let settings = item.explorer.clone().unwrap_or_default();
                    let Some(root) = settings
                        .root
                        .clone()
                        .filter(|root| here(root))
                        .or(folder.clone())
                        .or_else(std::env::home_dir)
                    else {
                        continue;
                    };
                    let project_root = folder.clone();
                    let explorer = cx.new(|cx| ExplorerView::new(root, project_root, settings, server.clone(), cx));
                    let subscription = cx.subscribe(&explorer, move |this, _, event, cx| match event {
                        ExplorerEvent::SettingsChanged(settings) => {
                            let (project, settings) = (this.project_id, settings.clone());
                            this.store
                                .update(cx, |store, _| store.set_explorer_settings(project, id, settings));
                            this.save_soon(cx);
                        }
                        ExplorerEvent::OpenTerminal(folder) => cx.emit(CanvasEvent::OpenTerminal {
                            folder: folder.clone(),
                            next_to: id,
                        }),
                        ExplorerEvent::OpenFile(path) => cx.emit(CanvasEvent::OpenFile {
                            path: path.clone(),
                            next_to: id,
                        }),
                        ExplorerEvent::OpenExplorer(root) => cx.emit(CanvasEvent::OpenExplorer {
                            root: root.clone(),
                            next_to: id,
                        }),
                        ExplorerEvent::Menu { entries, position } => cx.emit(CanvasEvent::Menu {
                            item: id,
                            entries: entries.clone(),
                            position: *position,
                        }),
                    });
                    self.windows.insert(id, (Content::Explorer(explorer), vec![subscription]));
                }
                ItemKind::Lino => {
                    let Some(root) = folder.clone() else {
                        continue;
                    };
                    let lino = cx.new(|cx| LinoView::new(root, server.clone(), cx));
                    // Its header shows how lino is doing.
                    let subscription = cx.observe(&lino, |_, _, cx| cx.notify());
                    self.windows.insert(id, (Content::Lino(lino), vec![subscription]));
                }
                ItemKind::Editor => {
                    let Some(path) = item.path.clone() else {
                        continue;
                    };
                    let editor = cx.new(|cx| EditorView::new(path, server.clone(), cx));
                    let subscriptions = vec![
                        // Its header shows the language and unsaved changes.
                        cx.observe(&editor, |_, _, cx| cx.notify()),
                        cx.subscribe(&editor, move |_, _, event, cx| match event {
                            EditorEvent::OpenAt { path, line, character } => cx.emit(CanvasEvent::OpenFileAt {
                                path: path.clone(),
                                line: *line,
                                character: *character,
                                next_to: id,
                            }),
                        }),
                    ];
                    self.windows.insert(id, (Content::Editor(editor), subscriptions));
                }
                ItemKind::Git => {
                    let Some(root) = folder.clone() else {
                        continue;
                    };
                    let git = cx.new(|cx| GitView::new(root, server.clone(), cx));
                    let subscription = cx.subscribe(&git, move |_, _, event, cx| match event {
                        GitEvent::Menu { entries, position } => cx.emit(CanvasEvent::Menu {
                            item: id,
                            entries: entries.clone(),
                            position: *position,
                        }),
                        GitEvent::OpenFile(path) => cx.emit(CanvasEvent::OpenFile {
                            path: path.clone(),
                            next_to: id,
                        }),
                        GitEvent::Confirm {
                            title,
                            description,
                            confirm,
                            action,
                        } => cx.emit(CanvasEvent::Confirm {
                            title: title.clone(),
                            description: description.clone(),
                            confirm: confirm.clone(),
                            action: action.clone(),
                        }),
                    });
                    self.windows.insert(id, (Content::Git(git), vec![subscription]));
                }
            }
        }
        if self.active.is_some_and(|id| !self.windows.contains_key(&id)) {
            self.active = None;
        }
        cx.notify();
    }

    /// Writes the layout once it stops changing.
    fn save_soon(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        self.save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            store.read_with(cx, |store, _| store.save()).ok();
        }));
    }

    fn set_viewport(&mut self, viewport: Viewport, cx: &mut Context<Self>) {
        let project = self.project_id;
        self.store
            .update(cx, |store, _| store.set_viewport(project, viewport));
        self.save_soon(cx);
        cx.notify();
    }

    // Windows ----------------------------------------------------------------

    /// Where a window opens when there's no active one: the middle of the
    /// view.
    fn centered_placement(&self, cx: &App) -> Placement {
        let viewport = self.viewport(cx);
        let (width, height) = self.view_size();
        if width <= 0. {
            return Placement::At {
                x: viewport.x + 48.,
                y: viewport.y + 48.,
            };
        }
        let (width, height) = (width / viewport.zoom, height / viewport.zoom);
        Placement::At {
            x: viewport.x + (width - DEFAULT_WINDOW_WIDTH).max(0.) / 2.,
            y: viewport.y + (height - DEFAULT_WINDOW_HEIGHT).max(0.) / 2.,
        }
    }

    /// Opens a new terminal next to the active window (right, or `below`),
    /// or in the middle of the view.
    pub fn add_terminal(&mut self, below: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !Mods::installed(ItemKind::Terminal, cx) {
            return;
        }
        let placement = match (below, self.active) {
            (true, Some(active)) => Placement::Below(active),
            (false, Some(active)) => Placement::RightOf(active),
            (_, None) => self.centered_placement(cx),
        };
        let project = self.project_id;
        let id = self.store.update(cx, |store, cx| {
            let id = store.add_item(project, ItemKind::Terminal, placement);
            cx.notify();
            id
        });
        if let Some(id) = id {
            self.sync(cx);
            self.focus_item(id, window, cx);
        }
    }

    /// Opens a new terminal in `folder`, to the right of window `next_to`
    /// (the Explorer's "Open terminal here").
    pub fn add_terminal_in(&mut self, folder: PathBuf, next_to: u64, window: &mut Window, cx: &mut Context<Self>) {
        if !Mods::installed(ItemKind::Terminal, cx) {
            return;
        }
        let project = self.project_id;
        let id = self.store.update(cx, |store, cx| {
            let id = store.add_item(project, ItemKind::Terminal, Placement::RightOf(next_to));
            if let Some(id) = id {
                store.set_item_cwd(project, id, folder);
            }
            cx.notify();
            id
        });
        if let Some(id) = id {
            self.sync(cx);
            self.focus_item(id, window, cx);
        }
    }

    /// Opens `path` in an editor (the one it's open in already, or a new one
    /// to the right of window `next_to`) and focuses it.
    pub fn open_file(&mut self, path: PathBuf, next_to: u64, window: &mut Window, cx: &mut Context<Self>) {
        // Without the Editor mod, files open in their default app.
        if !Mods::installed(ItemKind::Editor, cx) {
            crate::windows::explorer::open_with_default_app(&path, false);
            return;
        }
        let project = self.project_id;
        let id = self.store.update(cx, |store, cx| {
            let id = store.open_file(project, path, Placement::RightOf(next_to));
            cx.notify();
            id
        });
        if let Some(id) = id {
            self.sync(cx);
            self.focus_item(id, window, cx);
        }
    }

    /// A new explorer, next to the active window (or in the middle of the
    /// view), showing the project's folder.
    pub fn new_explorer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placement = match self.active {
            Some(active) => Placement::RightOf(active),
            None => self.centered_placement(cx),
        };
        self.add_explorer(ExplorerSettings::default(), placement, window, cx);
    }

    /// A new explorer showing `root`, to the right of window `next_to`.
    pub fn open_explorer(&mut self, root: PathBuf, next_to: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        let placement = match next_to.or(self.active) {
            Some(id) => Placement::RightOf(id),
            None => self.centered_placement(cx),
        };
        let own_root = (self.project(cx).and_then(|p| p.folder.as_ref()) != Some(&root)).then_some(root);
        let settings = ExplorerSettings {
            root: own_root,
            ..ExplorerSettings::default()
        };
        self.add_explorer(settings, placement, window, cx);
    }

    /// A copy of an explorer (same folder, same view), to its right.
    fn duplicate_explorer(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let settings = match self.windows.get(&id) {
            Some((Content::Explorer(explorer), _)) => explorer.read(cx).settings().clone(),
            _ => self
                .project(cx)
                .and_then(|p| p.item(id))
                .and_then(|item| item.explorer.clone())
                .unwrap_or_default(),
        };
        self.add_explorer(settings, Placement::RightOf(id), window, cx);
    }

    fn add_explorer(&mut self, settings: ExplorerSettings, placement: Placement, window: &mut Window, cx: &mut Context<Self>) {
        if !Mods::installed(ItemKind::Explorer, cx) {
            return;
        }
        let project = self.project_id;
        let id = self.store.update(cx, |store, cx| {
            let id = store.add_explorer(project, settings, placement);
            cx.notify();
            id
        });
        if let Some(id) = id {
            self.sync(cx);
            self.focus_item(id, window, cx);
        }
    }

    /// Files and folders dropped on the canvas: files open in editors,
    /// folders in explorers, next to window `next_to` or where they were
    /// dropped.
    pub fn open_paths(&mut self, paths: Vec<PathBuf>, next_to: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        let mut placement = match next_to {
            Some(id) => Placement::RightOf(id),
            None => {
                let viewport = self.viewport(cx);
                let origin = self.bounds.get().origin;
                let mouse = window.mouse_position();
                Placement::At {
                    x: viewport.x + f32::from(mouse.x - origin.x) / viewport.zoom,
                    y: viewport.y + f32::from(mouse.y - origin.y) / viewport.zoom,
                }
            }
        };
        let project = self.project_id;
        let folder = self.project(cx).and_then(|p| p.folder.clone());
        let mut last = None;
        let (explorers, editors) = (Mods::installed(ItemKind::Explorer, cx), Mods::installed(ItemKind::Editor, cx));
        for path in paths {
            let is_dir = if self.server.is_some() { path.extension().is_none() } else { path.is_dir() };
            if !(if is_dir { explorers } else { editors }) {
                if !is_dir && self.server.is_none() {
                    crate::windows::explorer::open_with_default_app(&path, false);
                }
                continue;
            }
            let id = self.store.update(cx, |store, cx| {
                let id = if is_dir {
                    let root = (folder.as_ref() != Some(&path)).then_some(path);
                    store.add_explorer(project, ExplorerSettings { root, ..ExplorerSettings::default() }, placement)
                } else {
                    store.open_file(project, path, placement)
                };
                cx.notify();
                id
            });
            if let Some(id) = id {
                // Several open in a row.
                placement = Placement::RightOf(id);
                last = Some(id);
            }
        }
        if let Some(id) = last {
            self.sync(cx);
            self.focus_item(id, window, cx);
        }
    }

    /// Opens `path` in an editor and puts the caret at `line`/`character`.
    pub fn open_file_at(&mut self, path: PathBuf, line: usize, character: usize, next_to: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.open_file(path, next_to, window, cx);
        if let Some(active) = self.active
            && let Some((Content::Editor(editor), _)) = self.windows.get(&active)
        {
            editor.update(cx, |editor, cx| editor.go_to(line, character, cx));
        }
    }

    /// Closes windows whose mod was uninstalled (editors with unsaved
    /// changes stay open).
    pub fn close_uninstalled(&mut self, cx: &mut Context<Self>) {
        let doomed: Vec<u64> = self
            .project(cx)
            .map(|project| {
                project
                    .windows
                    .iter()
                    .filter_map(|frame| project.item(frame.item))
                    .filter(|item| !Mods::installed(item.kind, cx))
                    .map(|item| item.id)
                    .collect()
            })
            .unwrap_or_default();
        for id in doomed {
            let dirty = matches!(self.windows.get(&id), Some((Content::Editor(editor), _)) if editor.read(cx).dirty());
            if !dirty {
                self.discard_item(id, cx);
            }
        }
    }

    /// The names of files open with unsaved changes.
    pub fn unsaved_files(&self, cx: &App) -> Vec<String> {
        self.windows
            .values()
            .filter_map(|(content, _)| match content {
                Content::Editor(editor) if editor.read(cx).dirty() => editor
                    .read(cx)
                    .path()
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    }

    /// Shows an item: opens it if it's closed, then focuses it and brings it
    /// into view.
    pub fn open_item(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let placement = match self.active {
            Some(active) => Placement::RightOf(active),
            None => self.centered_placement(cx),
        };
        let project = self.project_id;
        self.store.update(cx, |store, cx| {
            store.open_item(project, id, placement);
            cx.notify();
        });
        self.sync(cx);
        self.focus_item(id, window, cx);
    }

    pub fn focus_item(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.activate(id, window, cx);
        self.reveal(id, cx);
    }

    /// Makes `id` the active window: in front, with the keyboard.
    fn activate(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.active = Some(id);
        let project = self.project_id;
        if self.store.update(cx, |store, _| store.raise(project, id)) {
            self.save_soon(cx);
        }
        if let Some((content, _)) = self.windows.get(&id)
            && !content.contains_focus(window, cx)
        {
            content.focus(window, cx);
        }
        cx.notify();
    }

    /// Pans (and zooms out if needed) so the window is fully in view.
    fn reveal(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(frame) = self.project(cx).and_then(|p| p.frame(id)).copied() else {
            return;
        };
        let viewport = self.viewport(cx);
        let (width, height) = self.view_size();
        if width <= 0. {
            return;
        }
        let (view_w, view_h) = (width / viewport.zoom, height / viewport.zoom);
        let inside = frame.x >= viewport.x
            && frame.y >= viewport.y
            && frame.x + frame.width <= viewport.x + view_w
            && frame.y + frame.height <= viewport.y + view_h;
        if inside {
            return;
        }
        let next = if frame.width <= view_w && frame.height <= view_h {
            // Centre it at the current zoom.
            Viewport {
                x: frame.x + frame.width / 2. - view_w / 2.,
                y: frame.y + frame.height / 2. - view_h / 2.,
                zoom: viewport.zoom,
            }
        } else {
            match fit(&[frame], (width, height), viewport.zoom) {
                Some(next) => next,
                None => return,
            }
        };
        self.set_viewport(next, cx);
    }

    /// Gives the keyboard to the active window, or to the canvas when no
    /// window is open (so ⌘T still works).
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active.and_then(|id| self.windows.get(&id)) {
            Some((content, _)) => content.focus(window, cx),
            None => window.focus(&self.focus_handle),
        }
    }

    /// Ends every shell in the project (it's being deleted).
    pub fn kill_all(&self, cx: &App) {
        for (content, _) in self.windows.values() {
            content.kill(cx);
        }
    }

    /// Closes a window, ending its shell. The window stays in the project;
    /// opening it again starts a new shell where this one was. An editor
    /// leaves the project (and asks first if it has unsaved changes).
    pub fn close_item(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some((Content::Editor(editor), _)) = self.windows.get(&id) {
            let editor = editor.read(cx);
            if editor.dirty() {
                let name = editor.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                cx.emit(CanvasEvent::ConfirmDiscard { item: id, name });
                return;
            }
        }
        self.discard_item(id, cx);
    }

    /// Closes a window without asking, losing an editor's unsaved changes.
    /// Only the permanent windows (the first Explorer, Lino, Git) stay in
    /// the project, closed; the rest leave it (the sidebar lists only
    /// what's open).
    pub fn discard_item(&mut self, id: u64, cx: &mut Context<Self>) {
        let permanent = self.project(cx).is_some_and(|p| p.is_permanent(id));
        if !permanent {
            self.remove_item(id, cx);
            return;
        }
        if let Some((content, _)) = self.windows.get(&id) {
            content.kill(cx);
        }
        let next = self.next_active(id, cx);
        let project = self.project_id;
        self.store.update(cx, |store, cx| {
            store.close_item(project, id);
            cx.notify();
        });
        if self.active == Some(id) {
            self.active = next;
        }
        self.sync(cx);
    }

    /// Opens a copy of a terminal (a new shell in its folder) or an
    /// explorer, to its right.
    pub fn duplicate_item(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.project(cx).and_then(|p| p.item(id)).cloned() else {
            return;
        };
        match item.kind {
            ItemKind::Terminal => {
                let live = match self.windows.get(&id) {
                    Some((Content::Terminal(terminal), _)) => terminal.read(cx).cwd(),
                    _ => None,
                };
                let folder = live
                    .or(item.cwd)
                    .or_else(|| self.project(cx).and_then(|p| p.folder.clone()))
                    .or_else(std::env::home_dir);
                if let Some(folder) = folder {
                    self.add_terminal_in(folder, id, window, cx);
                }
            }
            ItemKind::Explorer => self.duplicate_explorer(id, window, cx),
            _ => {}
        }
    }

    pub fn remove_item(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some((content, _)) = self.windows.get(&id) {
            content.kill(cx);
        }
        let next = self.next_active(id, cx);
        let project = self.project_id;
        self.store.update(cx, |store, cx| {
            store.remove_item(project, id);
            cx.notify();
        });
        if self.active == Some(id) {
            self.active = next;
        }
        self.sync(cx);
    }

    /// Who takes over when `id` closes: the frontmost other window.
    fn next_active(&self, id: u64, cx: &App) -> Option<u64> {
        let project = self.project(cx)?;
        project
            .windows
            .iter()
            .rev()
            .map(|frame| frame.item)
            .find(|&item| item != id)
    }

    // Gestures ---------------------------------------------------------------

    fn begin_gesture(&mut self, mouse: Point<Pixels>, item: Option<u64>, cx: &App) {
        self.gesture = Some(Gesture {
            mouse,
            frame: item.and_then(|item| self.project(cx)?.frame(item).copied()),
            viewport: self.viewport(cx),
        });
    }

    /// The pointer's travel since the gesture began, in canvas units.
    fn travel(gesture: &Gesture, mouse: Point<Pixels>) -> (f32, f32) {
        let zoom = gesture.viewport.zoom;
        (
            f32::from(mouse.x - gesture.mouse.x) / zoom,
            f32::from(mouse.y - gesture.mouse.y) / zoom,
        )
    }

    fn drag_window(&mut self, item: u64, edge: Option<Edge>, mouse: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(gesture) = self.gesture else {
            return;
        };
        let Some(start) = gesture.frame.filter(|frame| frame.item == item) else {
            return;
        };
        let (dx, dy) = Self::travel(&gesture, mouse);
        let next = match edge {
            None => Frame {
                x: start.x + dx,
                y: start.y + dy,
                ..start
            },
            Some(edge) => resized(start, edge, dx, dy),
        };
        let project = self.project_id;
        self.store.update(cx, |store, _| {
            store.set_frame(project, item, next.x, next.y, next.width, next.height)
        });
        self.save_soon(cx);
        cx.notify();
    }

    fn drag_pan(&mut self, mouse: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(gesture) = self.gesture else {
            return;
        };
        let (dx, dy) = Self::travel(&gesture, mouse);
        self.set_viewport(
            Viewport {
                x: gesture.viewport.x - dx,
                y: gesture.viewport.y - dy,
                ..gesture.viewport
            },
            cx,
        );
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let viewport = self.viewport(cx);
        let delta = event.delta.pixel_delta(px(20.));
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        if event.modifiers.control || event.modifiers.platform {
            let origin = self.bounds.get().origin;
            let anchor = point(
                f32::from(event.position.x - origin.x),
                f32::from(event.position.y - origin.y),
            );
            let zoom = viewport.zoom * (dy / 240.).exp();
            self.set_viewport(zoom_around(viewport, anchor, zoom), cx);
        } else {
            self.set_viewport(
                Viewport {
                    x: viewport.x - dx / viewport.zoom,
                    y: viewport.y - dy / viewport.zoom,
                    ..viewport
                },
                cx,
            );
        }
    }

    /// A trackpad pinch (`factor` > 1 zooms in), around the pointer at
    /// `position` (window coordinates). Ignored when the pointer isn't over
    /// the canvas.
    pub fn pinch(&mut self, factor: f32, position: Point<Pixels>, cx: &mut Context<Self>) {
        let bounds = self.bounds.get();
        if !bounds.contains(&position) {
            return;
        }
        let anchor = point(
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        );
        let viewport = self.viewport(cx);
        self.set_viewport(zoom_around(viewport, anchor, viewport.zoom * factor), cx);
    }

    // Zoom actions -----------------------------------------------------------

    fn zoom_to(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let (width, height) = self.view_size();
        let center = point(width / 2., height / 2.);
        let viewport = zoom_around(self.viewport(cx), center, zoom);
        self.set_viewport(viewport, cx);
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        let zoom = self.viewport(cx).zoom * ZOOM_STEP;
        self.zoom_to(zoom, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        let zoom = self.viewport(cx).zoom / ZOOM_STEP;
        self.zoom_to(zoom, cx);
    }

    fn zoom_actual(&mut self, _: &ZoomToActualSize, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_to(1., cx);
    }

    /// Fits every window, never zooming in past 100%.
    fn zoom_fit(&mut self, _: &ZoomToFit, _: &mut Window, cx: &mut Context<Self>) {
        let frames = self.project(cx).map(|p| p.windows.clone()).unwrap_or_default();
        if let Some(viewport) = fit(&frames, self.view_size(), 1.) {
            self.set_viewport(viewport, cx);
        }
    }

    fn zoom_window(&mut self, _: &ZoomToWindow, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active {
            self.zoom_to_item(id, cx);
        }
    }

    fn zoom_to_item(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(frame) = self.project(cx).and_then(|p| p.frame(id)).copied() else {
            return;
        };
        if let Some(viewport) = fit(&[frame], self.view_size(), 2.) {
            self.set_viewport(viewport, cx);
        }
    }

    // Window actions -----------------------------------------------------------

    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.add_terminal(false, window, cx);
    }

    fn new_terminal_below(&mut self, _: &NewTerminalBelow, window: &mut Window, cx: &mut Context<Self>) {
        self.add_terminal(true, window, cx);
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active {
            self.close_item(id, cx);
            self.focus(window, cx);
        }
    }

    // Rendering --------------------------------------------------------------

    fn render_window(
        &self,
        frame: &Frame,
        item: &ProjectItem,
        viewport: Viewport,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = item.id;
        let zoom = viewport.zoom;
        let left = (frame.x - viewport.x) * zoom;
        let top = (frame.y - viewport.y) * zoom;

        div()
            .id(ElementId::named_usize("window", id as usize))
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(frame.width * zoom))
            .h(px(frame.height * zoom))
            // Clicks stay on the window; scrolling still reaches the
            // canvas (to pan) unless the terminal takes it.
            .block_mouse_except_scroll()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.activate(id, window, cx);
                    // The canvas is focusable too; don't let it take the
                    // keyboard back from the window.
                    window.prevent_default();
                }),
            )
            .on_drop(cx.listener(move |this, files: &DraggedFiles, window, cx| {
                this.open_paths(files.0.clone(), Some(id), window, cx)
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                if this.server.is_none() {
                    this.open_paths(paths.paths().to_vec(), Some(id), window, cx)
                }
            }))
            .child(Scaled {
                rem: px(16. * zoom),
                child: self.render_window_contents(item, theme, cx).into_any_element(),
            })
            .children(Edge::ALL.into_iter().map(|edge| {
                resize_handle(edge)
                    .id(ElementId::named_usize("resize", id as usize * 8 + edge as usize))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.begin_gesture(event.position, Some(id), cx)
                        }),
                    )
                    .on_drag(ResizeWindow { item: id, edge }, move |_, _, window, cx| {
                        cx.set_active_drag_cursor_style(edge.cursor(), window);
                        cx.new(|_| NoPreview)
                    })
            }))
            .into_any_element()
    }

    /// A window's chrome and content, sized in rems so [`Scaled`] zooms it.
    fn render_window_contents(&self, item: &ProjectItem, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let id = item.id;
        let active = self.active == Some(id);
        let content = self.windows.get(&id).map(|(content, _)| content.clone());
        // A command running in a terminal, or lino's live process.
        let running = match &content {
            Some(Content::Terminal(terminal)) => terminal.read(cx).running(),
            Some(Content::Lino(lino)) => lino.read(cx).running(),
            _ => false,
        };
        // What the shell says: its folder at a prompt (shown with ~), or
        // the command that's running.
        let live_title = match &content {
            Some(Content::Terminal(terminal)) => {
                let terminal = terminal.read(cx);
                match terminal.cwd() {
                    Some(cwd) => Some(ui::display_path(&cwd)),
                    None => terminal.title().map(|title| title.to_string()),
                }
            }
            // An explorer shows its folder.
            Some(Content::Explorer(explorer)) => Some(ui::display_path(explorer.read(cx).root())),
            Some(Content::Lino(lino)) => lino.read(cx).summary(),
            Some(Content::Editor(editor)) => editor.read(cx).summary(),
            Some(Content::Git(git)) => git.read(cx).summary(),
            None => None,
        };
        // Windows that only react once they're active (terminals check
        // their focus themselves).
        let covered = matches!(content, Some(Content::Explorer(_) | Content::Lino(_) | Content::Git(_)));
        let duplicable = matches!(item.kind, ItemKind::Terminal | ItemKind::Explorer);
        let accent = item.color.resolve(&theme.tints, theme.muted_foreground);

        // A window: border bg-background rounded-xl, border-active while
        // it's the active one.
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(rems(14. / 16.))
            .border_1()
            .border_color(if active { theme.border_active } else { theme.border })
            .bg(theme.background)
            .child(
                // Header (the drag handle): h-8 px-3 gap-2 text-xs, border-b.
                div()
                    .id(ElementId::named_usize("window-header", id as usize))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .h_8()
                    .pl_3()
                    .pr_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.card_header)
                    // GPUI clips children to a rectangle, not to the window's
                    // rounded corners, so the header rounds its own (the
                    // window's radius, less its 1px border).
                    .rounded_t(rems(13. / 16.))
                    .text_size(rems(0.75))
                    .line_height(rems(1.))
                    .text_color(theme.muted_foreground)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.begin_gesture(event.position, Some(id), cx)
                        }),
                    )
                    .on_drag(MoveWindow(id), |_, _, window, cx| {
                        cx.set_active_drag_cursor_style(CursorStyle::ClosedHand, window);
                        cx.new(|_| NoPreview)
                    })
                    // Double-click zooms to the window, as in Figma.
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                        if event.click_count() == 2 {
                            this.zoom_to_item(id, cx);
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                            cx.emit(CanvasEvent::ItemMenu {
                                item: id,
                                position: event.position,
                            });
                        }),
                    )
                    .child(ui::icon(item.kind.icon()).size_3p5().text_color(accent))
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if active {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .child(item.title.clone()),
                    )
                    // Something is running: a command (keys go straight to it), or lino.
                    .when(running, |header| header.child(ui::status_dot(theme.tints.blue.base)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.muted_foreground.opacity(0.7))
                            .children(live_title.map(SharedString::from)),
                    )
                    .child(
                        header_button(id, "menu", IconName::Ellipsis, theme).on_click(cx.listener(
                            move |_, event: &gpui::ClickEvent, _, cx| {
                                cx.stop_propagation();
                                cx.emit(CanvasEvent::ItemMenu {
                                    item: id,
                                    position: event.position(),
                                });
                            },
                        )),
                    )
                    // Terminals and explorers can be duplicated: the copy
                    // opens to the right, in the same folder.
                    .when(duplicable, |header| {
                        header.child(
                            header_button(id, "duplicate", IconName::CopyPlus, theme).on_click(cx.listener(
                                move |this, _: &gpui::ClickEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.duplicate_item(id, window, cx);
                                },
                            )),
                        )
                    })
                    .child(
                        header_button(id, "close", IconName::X, theme).on_click(cx.listener(
                            move |this, _: &gpui::ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.close_item(id, cx);
                                this.focus(window, cx);
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    // Cached: a window only renders again when it changed
                    // (or moved), so a busy terminal doesn't make every other
                    // window render with it.
                    .children(content.as_ref().map(|content| {
                        content.view().cached(gpui::StyleRefinement::default().size_full())
                    }))
                    // An Explorer or Lino window that isn't active doesn't
                    // take clicks or scrolling: the first click picks the
                    // window, and scrolling over it pans the canvas, as over
                    // any other window.
                    // While files are dragged, every explorer takes drops,
                    // active or not (another explorer, Finder).
                    .when(covered && !active && !cx.has_active_drag(), |body| {
                        body.child(
                            div()
                                .id(ElementId::named_usize("window-cover", id as usize))
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .occlude()
                                .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.activate(id, window, cx);
                                        window.prevent_default();
                                    }),
                                ),
                        )
                    }),
            )
    }

    fn render_zoom_controls(&self, zoom: f32, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let hover_bg = theme.secondary;
        // A floating pill, bottom right: − 100% + | fit
        div()
            .id("zoom-controls")
            .absolute()
            .right(px(12.))
            .bottom(px(12.))
            .flex()
            .items_center()
            .gap(px(2.))
            .p(px(4.))
            .rounded(radius::LG)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .block_mouse_except_scroll()
            // Zooming leaves the keyboard where it was.
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .child(
                ui::small_icon_button("zoom-out", IconName::Minus, theme)
                    .on_click(cx.listener(|this, _, window, cx| this.zoom_out(&ZoomOut, window, cx))),
            )
            .child(
                div()
                    .id("zoom-reset")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(48.))
                    .h_6()
                    .rounded(radius::SM)
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .hover(move |style| style.bg(hover_bg))
                    .child(format!("{}%", (zoom * 100.).round()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.zoom_actual(&ZoomToActualSize, window, cx)
                    })),
            )
            .child(
                ui::small_icon_button("zoom-in", IconName::Plus, theme)
                    .on_click(cx.listener(|this, _, window, cx| this.zoom_in(&ZoomIn, window, cx))),
            )
            .child(div().w_px().h(px(16.)).mx(px(2.)).bg(theme.border))
            .child(
                ui::small_icon_button("zoom-fit", IconName::Maximize, theme)
                    .on_click(cx.listener(|this, _, window, cx| this.zoom_fit(&ZoomToFit, window, cx))),
            )
    }

    fn render_empty(&self, project: &Project, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let closed = !project.items.is_empty();
        // No hitbox of its own, so empty canvas around the text still pans.
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(16.))
                    .line_height(px(24.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("No windows open"),
            )
            .child(
                div()
                    .max_w(px(380.))
                    .text_center()
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .text_color(theme.muted_foreground)
                    .child(if closed {
                        "Reopen a window from the sidebar, or start a new terminal."
                    } else {
                        "Windows sit anywhere on this canvas. Drag empty space to pan, ⌘-scroll to zoom."
                    }),
            )
            .child(
                div().mt_3().child(
                    Button::new("empty-new-terminal", "New terminal")
                        .icon(IconName::Plus)
                        .on_click(cx.listener(|this, _, window, cx| this.add_terminal(false, window, cx))),
                ),
            )
    }
}

impl Render for ProjectCanvas {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let Some(project) = self.project(cx) else {
            return div().into_any_element();
        };
        let viewport = project.viewport;
        let bounds = self.bounds.clone();
        let dot_color = theme.border;
        let (view_w, view_h) = self.view_size();

        // Windows outside the view aren't drawn (their sessions keep
        // running). The active one always is, so it keeps the keyboard.
        let visible = |frame: &Frame| {
            if view_w <= 0. || self.active == Some(frame.item) {
                return true;
            }
            let left = (frame.x - viewport.x) * viewport.zoom;
            let top = (frame.y - viewport.y) * viewport.zoom;
            left < view_w
                && top < view_h
                && left + frame.width * viewport.zoom > 0.
                && top + frame.height * viewport.zoom > 0.
        };

        div()
            .id("canvas")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::new_terminal_below))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_actual))
            .on_action(cx.listener(Self::zoom_fit))
            .on_action(cx.listener(Self::zoom_window))
            .relative()
            .size_full()
            .overflow_hidden()
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<MoveWindow>, _, cx| {
                let item = event.drag(cx).0;
                this.drag_window(item, None, event.event.position, cx);
            }))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<ResizeWindow>, _, cx| {
                let drag = event.drag(cx);
                let (item, edge) = (drag.item, drag.edge);
                this.drag_window(item, Some(edge), event.event.position, cx);
            }))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<PanCanvas>, _, cx| {
                this.drag_pan(event.event.position, cx);
            }))
            .child(
                // Empty canvas: a dot grid that pans when dragged.
                div()
                    .id("canvas-background")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.begin_gesture(event.position, None, cx);
                            // Clicking empty canvas lets go of every window,
                            // as in Figma: none is active or takes typing.
                            // The canvas keeps the keyboard, for ⌘T and zoom.
                            this.active = None;
                            window.focus(&this.focus_handle);
                            window.prevent_default();
                            cx.notify();
                        }),
                    )
                    .on_drag(PanCanvas, |_, _, window, cx| {
                        cx.set_active_drag_cursor_style(CursorStyle::ClosedHand, window);
                        cx.new(|_| NoPreview)
                    })
                    // Files dropped on empty canvas open where they land.
                    .on_drop(cx.listener(|this, files: &DraggedFiles, window, cx| {
                        this.open_paths(files.0.clone(), None, window, cx)
                    }))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                        if this.server.is_none() {
                            this.open_paths(paths.paths().to_vec(), None, window, cx)
                        }
                    }))
                    .child(
                        canvas(
                            move |view, _, _| bounds.set(view),
                            move |view, _, window, _| paint_dots(view, viewport, dot_color, window),
                        )
                        .size_full(),
                    ),
            )
            .children(
                project
                    .windows
                    .iter()
                    .filter(|frame| visible(frame))
                    .filter_map(|frame| {
                        let item = project.item(frame.item)?;
                        Some(self.render_window(frame, item, viewport, theme, cx))
                    }),
            )
            .when(project.windows.is_empty(), |canvas| {
                canvas.child(self.render_empty(project, theme, cx))
            })
            .child(self.render_zoom_controls(viewport.zoom, theme, cx))
            .into_any_element()
    }
}

/// The most dots the grid paints. Every dot is a quad painted each frame,
/// and a wide screen holds tens of thousands at the closest spacing.
const MAX_DOTS: f32 = 2400.;

/// The canvas's dot grid: a dot every 24 units, thinned out (doubling the
/// spacing) whenever dots would sit closer than 16px, or be too many.
fn paint_dots(bounds: Bounds<Pixels>, viewport: Viewport, color: Hsla, window: &mut Window) {
    let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let mut step = 24.;
    while step * viewport.zoom < 16. || (width * height) / (step * viewport.zoom).powi(2) > MAX_DOTS {
        step *= 2.;
    }
    let spacing = step * viewport.zoom;
    let dot = if viewport.zoom >= 0.5 { 2. } else { 1.5 };
    let first_x = (-viewport.x * viewport.zoom).rem_euclid(spacing);
    let first_y = (-viewport.y * viewport.zoom).rem_euclid(spacing);
    let mut y = first_y;
    while y < height {
        let mut x = first_x;
        while x < width {
            let origin = point(
                bounds.origin.x + px(x - dot / 2.),
                bounds.origin.y + px(y - dot / 2.),
            );
            window.paint_quad(fill(Bounds::new(origin, size(px(dot), px(dot))), color).corner_radii(px(dot / 2.)));
            x += spacing;
        }
        y += spacing;
    }
}

/// An invisible hit area along a window edge or corner, in screen px (it
/// doesn't scale with zoom, so it's always easy to grab).
fn resize_handle(edge: Edge) -> gpui::Div {
    const EDGE: f32 = 8.;
    const CORNER: f32 = 14.;
    let handle = div().absolute().cursor(edge.cursor());
    match edge {
        Edge::Left => handle.left(px(-EDGE / 2.)).top(px(CORNER / 2.)).bottom(px(CORNER / 2.)).w(px(EDGE)),
        Edge::Right => handle.right(px(-EDGE / 2.)).top(px(CORNER / 2.)).bottom(px(CORNER / 2.)).w(px(EDGE)),
        Edge::Top => handle.top(px(-EDGE / 2.)).left(px(CORNER / 2.)).right(px(CORNER / 2.)).h(px(EDGE)),
        Edge::Bottom => handle.bottom(px(-EDGE / 2.)).left(px(CORNER / 2.)).right(px(CORNER / 2.)).h(px(EDGE)),
        Edge::TopLeft => handle.top(px(-CORNER / 2.)).left(px(-CORNER / 2.)).size(px(CORNER)),
        Edge::TopRight => handle.top(px(-CORNER / 2.)).right(px(-CORNER / 2.)).size(px(CORNER)),
        Edge::BottomLeft => handle.bottom(px(-CORNER / 2.)).left(px(-CORNER / 2.)).size(px(CORNER)),
        Edge::BottomRight => handle.bottom(px(-CORNER / 2.)).right(px(-CORNER / 2.)).size(px(CORNER)),
    }
}

fn header_button(id: u64, name: &'static str, icon: IconName, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    ui::small_icon_button(ElementId::NamedInteger(name.into(), id), icon, theme)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::projects::{MAX_ZOOM, MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH, MIN_ZOOM};

    fn frame(x: f32, y: f32, width: f32, height: f32) -> Frame {
        Frame {
            item: 1,
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn resizing_moves_only_the_dragged_sides() {
        let f = frame(100., 100., 600., 400.);
        assert_eq!(resized(f, Edge::Right, 50., 999.), frame(100., 100., 650., 400.));
        assert_eq!(resized(f, Edge::Bottom, 999., -30.), frame(100., 100., 600., 370.));
        // Left and top keep the opposite side still.
        assert_eq!(resized(f, Edge::TopLeft, -20., 10.), frame(80., 110., 620., 390.));
        assert_eq!(resized(f, Edge::BottomLeft, 30., 25.), frame(130., 100., 570., 425.));
    }

    #[test]
    fn resizing_stops_at_the_minimum_size() {
        let f = frame(0., 0., 600., 400.);
        let small = resized(f, Edge::BottomRight, -10_000., -10_000.);
        assert_eq!((small.width, small.height), (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT));
        // Dragging the left edge too far pins the right edge, not the left.
        let pinned = resized(f, Edge::Left, 10_000., 0.);
        assert_eq!((pinned.x + pinned.width, pinned.width), (600., MIN_WINDOW_WIDTH));
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let viewport = Viewport { x: 100., y: 50., zoom: 1. };
        let anchor = point(300., 200.);
        let under = (viewport.x + anchor.x / viewport.zoom, viewport.y + anchor.y / viewport.zoom);
        let zoomed = zoom_around(viewport, anchor, 2.);
        assert_eq!(zoomed.zoom, 2.);
        assert_eq!((zoomed.x + anchor.x / zoomed.zoom, zoomed.y + anchor.y / zoomed.zoom), under);
        assert_eq!(zoom_around(viewport, anchor, 1000.).zoom, MAX_ZOOM);
        assert_eq!(zoom_around(viewport, anchor, 0.).zoom, MIN_ZOOM);
    }

    #[test]
    fn fitting_centres_everything() {
        let frames = [frame(0., 0., 500., 300.), frame(600., 100., 400., 400.)];
        // 1000 × 500 units into a 1096 × 596 view: zoom 1 fits with padding.
        let viewport = fit(&frames, (1096., 596.), 1.).unwrap();
        assert_eq!(viewport.zoom, 1.);
        assert_eq!((viewport.x, viewport.y), (500. - 548., 250. - 298.));
        // A smaller view zooms out.
        let small = fit(&frames, (596., 346.), 1.).unwrap();
        assert!((small.zoom - 0.5).abs() < 1e-6);
        assert_eq!(fit(&[], (100., 100.), 1.), None);
    }
}
