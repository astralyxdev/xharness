//! The app's window: the title bar, the sidebar (pages and projects), and
//! the panel that shows a page or a project's canvas. It owns the menus and
//! dialogs, which canvases ask it to show.

mod footer;
mod home;
pub mod marketplace;
pub mod project_dialog;
pub mod servers;
mod sidebar;

use std::{collections::HashMap, path::PathBuf};

use gpui::{
    AnyElement, AnyView, App, ClickEvent, Context, DismissEvent, ElementId, Entity, EntityId, Focusable,
    FontWeight, MouseButton, MouseDownEvent, Pixels, Point, Subscription, Window, actions, anchored, deferred,
    WindowControlArea, div, prelude::*, px, relative,
};

use crate::ui::actions::{Cancel, Confirm};
use crate::ui::alert_dialog::{AlertDialog, AlertDialogEvent};
use crate::ui::assets::{self, IconName};
use crate::canvas::{CanvasEvent, ProjectCanvas};
use crate::ui::context_menu::{ContextMenu, MenuEntry};
use project_dialog::{DialogTarget, ProjectDialog, ProjectDialogEvent};
use crate::services::projects::{ItemKind, Project, ProjectItem, ProjectStore};
use crate::services::{lino, server};
use crate::windows::Mods;
#[cfg(target_os = "macos")]
use crate::platform::macos;
use crate::ui::text_input::TextInput;
use crate::ui::theme::{FONT_MONO, Mode, Theme, radius};
use crate::ui::{self, Button, ButtonSize, Variant};

/// Window gutter: `p-2` around the frame and `gap-2` between rail and panel.
const GUTTER: f32 = 8.;
/// The title bar: as tall as the native one (measured on this macOS
/// version), so the traffic lights sit centred in it, and all of it drags
/// the window.
const TITLEBAR_HEIGHT: f32 = 32.;
/// The title bar's color: the logo's blue.
const TITLEBAR_COLOR: u32 = 0x1d32ad;
/// `w-64`
const SIDEBAR_WIDTH: f32 = 256.;
/// `px-2` on SidebarContent.
const SIDEBAR_PADDING_X: f32 = 8.;
/// Padding around the sidebar's logo. With the window gutter (left, top)
/// or the sidebar's gap (below), the logo has 24px on each of those sides.
const LOGO_INSET: f32 = SIDEBAR_PADDING_X + 8.;
/// Where a menu button's label starts: 10px inset + 16px icon + 8px gap.
/// Nested rows indent to here so they line up under their parent's name.
const MENU_LABEL_INSET: f32 = 10. + 16. + 8.;

actions!(app, [Quit]);

/// The app's name, as the window and menus show it.
pub const APP_TITLE: &str = "XHarness (Beta)";

/// Key context of the inline rename field: Enter saves, Escape cancels.
pub const RENAME_CONTEXT: &str = "ProjectRename";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Home,
    /// Language servers to install.
    Marketplace,
}

impl Page {
    /// Sidebar order.
    const ALL: [Page; 2] = [Page::Home, Page::Marketplace];

    fn label(self) -> &'static str {
        match self {
            Page::Home => "Home",
            Page::Marketplace => "Marketplace",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Page::Home => IconName::House,
            Page::Marketplace => IconName::Store,
        }
    }
}

/// What the panel on the right shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selection {
    Page(Page),
    /// A project's canvas.
    Project(u64),
}

/// What an open menu belongs to (that row stays highlighted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuTarget {
    Project(u64),
    Item(u64),
    /// A project's "New window" row.
    NewWindow(u64),
}

/// An open menu, shown at `position` (window coordinates).
struct OpenMenu {
    menu: Entity<ContextMenu>,
    target: MenuTarget,
    position: Point<Pixels>,
    _subscription: Subscription,
}

/// The open dialog (project dialog or delete confirmation).
struct Modal {
    view: AnyView,
    _subscription: Subscription,
}

/// A project being renamed in place in the sidebar.
struct Rename {
    project_id: u64,
    input: Entity<TextInput>,
    _blur: Subscription,
}

pub struct AppShell {
    selection: Selection,
    /// Shared with every canvas, which edit their project's layout in it.
    projects: Entity<ProjectStore>,
    /// One canvas per project opened so far; it owns that project's live
    /// windows, so they keep running while you look at another project.
    canvases: HashMap<u64, (Entity<ProjectCanvas>, [Subscription; 2])>,
    menu: Option<OpenMenu>,
    modal: Option<Modal>,
    rename: Option<Rename>,
    /// `None` follows the macOS appearance; `Some` is a manual choice.
    mode_override: Option<Mode>,
    marketplace: Entity<marketplace::MarketplaceView>,
    /// Which servers answer (this machine's, each remote project's).
    servers: Entity<servers::Servers>,
    /// Servers that answered at the last check: one that comes back gets
    /// its projects read again.
    connected: std::collections::HashSet<Option<server::ServerKey>>,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let projects = cx.new(|_| ProjectStore::load());
        let servers = cx.new(servers::Servers::new);
        let remotes: Vec<server::ServerKey> = projects.read(cx).projects().iter().filter_map(|p| p.remote.clone()).collect();
        servers.update(cx, |servers, cx| servers.watch(remotes, cx));
        let subscriptions = vec![
            cx.observe(&servers, |this, servers, cx| this.servers_changed(servers, cx)),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.apply_theme(window, cx);
            }),
            cx.observe(&projects, |_, _, cx| cx.notify()),
            // An uninstalled mod's windows close everywhere.
            cx.observe_global::<Mods>(|this, cx| {
                for (canvas, _) in this.canvases.values() {
                    canvas.update(cx, |canvas, cx| canvas.close_uninstalled(cx));
                }
                cx.notify();
            }),
            // Drags, panning and shell folders are saved a moment after they
            // settle; quitting mustn't lose the last of them.
            cx.on_app_quit(|this, cx| {
                this.projects.read(cx).save();
                async {}
            }),
        ];

        // Trackpad pinches, which GPUI doesn't see, zoom the canvas. They
        // come many times a second; apply whatever has queued up at once.
        #[cfg(target_os = "macos")]
        {
            use futures::StreamExt;
            let mut pinches = macos::pinch_events();
            cx.spawn_in(window, async move |this, cx| {
                while let Some(first) = pinches.next().await {
                    let mut factor = 1. + first;
                    while let Ok(next) = pinches.try_recv() {
                        factor *= 1. + next;
                    }
                    let pinched = this.update_in(cx, |this, window, cx| this.pinch(factor, window, cx));
                    if pinched.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        // Closing the window (the red button) asks about unsaved files too.
        let shell = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            shell.update(cx, |this, cx| this.quit(window, cx)).unwrap_or(true)
        });
        let this = Self {
            selection: Selection::Page(Page::Home),
            projects,
            canvases: HashMap::new(),
            menu: None,
            modal: None,
            rename: None,
            mode_override: None,
            marketplace: marketplace::new(cx),
            servers,
            connected: Default::default(),
            _subscriptions: subscriptions,
        };
        this.apply_theme(window, cx);
        this
    }

    fn apply_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self
            .mode_override
            .unwrap_or_else(|| Mode::from(window.appearance()));
        cx.set_global(Theme::for_mode(mode));
        // Windows on the canvas are cached views; a refresh renders them all
        // again, in the new colors.
        window.refresh();
        cx.notify();
    }

    /// A server came back (or went away): read its projects that couldn't be
    /// read, and keep watching every remote project's server.
    fn servers_changed(&mut self, servers: Entity<servers::Servers>, cx: &mut Context<Self>) {
        let keys: Vec<Option<server::ServerKey>> = std::iter::once(None)
            .chain(self.projects.read(cx).projects().iter().filter_map(|p| p.remote.clone()).map(Some))
            .collect();
        for key in keys {
            let connected = servers.read(cx).status(key.as_ref()).is_connected();
            if connected && !self.connected.contains(&key) {
                let address = key.clone().map(server::HostAddress::Remote);
                self.update_projects(cx, |store| store.reload(address.as_ref()));
                self.connected.insert(key);
            } else if !connected {
                self.connected.remove(&key);
            }
        }
        cx.notify();
    }

    /// Zooms the canvas on screen, around the pointer.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn pinch(&mut self, factor: f32, window: &mut Window, cx: &mut Context<Self>) {
        let Selection::Project(id) = self.selection else {
            return;
        };
        if self.modal.is_some() {
            return;
        }
        if let Some((canvas, _)) = self.canvases.get(&id) {
            let position = window.mouse_position();
            canvas.update(cx, |canvas, cx| canvas.pinch(factor, position, cx));
        }
    }

    /// Changes the store and re-renders whoever shows it.
    fn update_projects<R>(&self, cx: &mut Context<Self>, change: impl FnOnce(&mut ProjectStore) -> R) -> R {
        self.projects.update(cx, |store, cx| {
            let result = change(store);
            cx.notify();
            result
        })
    }

    fn project<'a>(&self, id: u64, cx: &'a App) -> Option<&'a Project> {
        self.projects.read(cx).get(id)
    }

    // Projects ---------------------------------------------------------------

    /// The project's canvas, created the first time it's needed. Creating it
    /// starts the project's open windows.
    fn canvas(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) -> Entity<ProjectCanvas> {
        if let Some((canvas, _)) = self.canvases.get(&id) {
            return canvas.clone();
        }
        let store = self.projects.clone();
        let canvas = cx.new(|cx| ProjectCanvas::new(id, store, cx));
        let subscriptions = [
            // The sidebar highlights the canvas's active window.
            cx.observe(&canvas, |_, _, cx| cx.notify()),
            // Window menus (from a window's header) are shown by the shell.
            cx.subscribe_in(&canvas, window, move |this, canvas, event, window, cx| match event {
                CanvasEvent::ItemMenu { item, position } => {
                    this.open_item_menu(id, *item, *position, window, cx)
                }
                CanvasEvent::Menu { item, entries, position } => {
                    this.show_menu(MenuTarget::Item(*item), entries.clone(), *position, window, cx)
                }
                CanvasEvent::OpenTerminal { folder, next_to } => {
                    let (folder, next_to) = (folder.clone(), *next_to);
                    canvas.update(cx, |canvas, cx| canvas.add_terminal_in(folder, next_to, window, cx));
                }
                CanvasEvent::OpenFile { path, next_to } => {
                    let (path, next_to) = (path.clone(), *next_to);
                    canvas.update(cx, |canvas, cx| canvas.open_file(path, next_to, window, cx));
                }
                CanvasEvent::OpenFileAt { path, line, character, next_to } => {
                    let (path, line, character, next_to) = (path.clone(), *line, *character, *next_to);
                    canvas.update(cx, |canvas, cx| canvas.open_file_at(path, line, character, next_to, window, cx));
                }
                CanvasEvent::OpenExplorer { root, next_to } => {
                    let (root, next_to) = (root.clone(), *next_to);
                    canvas.update(cx, |canvas, cx| canvas.open_explorer(root, Some(next_to), window, cx));
                }
                CanvasEvent::ConfirmDiscard { item, name } => {
                    this.confirm_discard(canvas.clone(), *item, name.clone(), window, cx);
                }
                CanvasEvent::Confirm { title, description, confirm, action } => {
                    let action = action.clone();
                    let alert = cx.new(|cx| AlertDialog::destructive(title.clone(), description.clone(), confirm.clone(), window, cx));
                    let subscription = cx.subscribe(&alert, move |this, alert, event, cx| {
                        if let AlertDialogEvent::Confirm = event {
                            action(cx);
                        }
                        this.close_modal(alert.entity_id(), cx);
                    });
                    this.modal = Some(Modal {
                        view: alert.into(),
                        _subscription: subscription,
                    });
                    cx.notify();
                }
            }),
        ];
        self.canvases.insert(id, (canvas.clone(), subscriptions));
        canvas
    }

    /// Shows the project's canvas and lists its windows in the sidebar.
    fn open_project(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = Selection::Project(id);
        self.update_projects(cx, |store| store.set_expanded(id, true));
        let canvas = self.canvas(id, window, cx);
        canvas.update(cx, |canvas, cx| canvas.focus(window, cx));
        cx.notify();
    }

    /// Shows one of a project's windows, reopening it if it's closed.
    fn open_item(&mut self, project: u64, item: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.selection = Selection::Project(project);
        let canvas = self.canvas(project, window, cx);
        canvas.update(cx, |canvas, cx| canvas.open_item(item, window, cx));
        cx.notify();
    }

    fn new_terminal(&mut self, project: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project(project, window, cx);
        let canvas = self.canvas(project, window, cx);
        canvas.update(cx, |canvas, cx| canvas.add_terminal(false, window, cx));
    }

    /// "New window": a menu of the windows a project can have.
    fn open_new_window_menu(&mut self, id: u64, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id, cx) else {
            return;
        };
        let has_folder = project.folder.is_some();
        let shell = cx.entity().downgrade();
        let action = |f: fn(&mut Self, u64, &mut Window, &mut Context<Self>)| {
            let shell = shell.clone();
            move |window: &mut Window, cx: &mut App| {
                shell.update(cx, |this, cx| f(this, id, window, cx)).ok();
            }
        };
        // Only installed mods (see the Marketplace).
        let installed = |kind| Mods::installed(kind, cx);
        let mut entries = Vec::new();
        if installed(ItemKind::Terminal) {
            entries.push(MenuEntry::item(IconName::SquareTerminal, "Terminal", action(Self::new_terminal)));
        }
        if installed(ItemKind::Explorer) {
            entries.push(MenuEntry::item(IconName::FolderTree, "Explorer", action(Self::open_explorer_window)));
        }
        if has_folder && installed(ItemKind::Git) {
            entries.push(MenuEntry::item(IconName::GitBranch, "Git", action(|this, id, window, cx| {
                this.open_kind(id, ItemKind::Git, window, cx)
            })));
        }
        if has_folder && installed(ItemKind::Lino) {
            entries.push(MenuEntry::item(IconName::Activity, "Lino", action(|this, id, window, cx| {
                this.open_kind(id, ItemKind::Lino, window, cx)
            })));
        }
        if entries.is_empty() {
            entries.push(MenuEntry::item(IconName::Store, "Get mods in the Marketplace", {
                let shell = shell.clone();
                move |_, cx| {
                    shell
                        .update(cx, |this, cx| {
                            this.selection = Selection::Page(Page::Marketplace);
                            cx.notify();
                        })
                        .ok();
                }
            }));
        }
        self.show_menu(MenuTarget::NewWindow(id), entries, position, window, cx);
    }

    /// Shows the project's window of `kind` (Git, Lino), opening it if it's
    /// closed.
    fn open_kind(&mut self, project: u64, kind: ItemKind, window: &mut Window, cx: &mut Context<Self>) {
        let item = self
            .project(project, cx)
            .and_then(|p| p.items.iter().find(|i| i.kind == kind))
            .map(|i| i.id);
        if let Some(item) = item {
            self.open_project(project, window, cx);
            self.open_item(project, item, window, cx);
        }
    }

    /// The project's Explorer if it's closed, else another one.
    fn open_explorer_window(&mut self, project: u64, window: &mut Window, cx: &mut Context<Self>) {
        let closed = self.project(project, cx).and_then(|p| {
            p.items
                .iter()
                .find(|i| i.kind == ItemKind::Explorer && p.is_permanent(i.id) && !p.is_open(i.id))
                .map(|i| i.id)
        });
        match closed {
            Some(item) => {
                self.open_project(project, window, cx);
                self.open_item(project, item, window, cx);
            }
            None => self.new_explorer(project, window, cx),
        }
    }

    fn new_explorer(&mut self, project: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project(project, window, cx);
        let canvas = self.canvas(project, window, cx);
        canvas.update(cx, |canvas, cx| canvas.new_explorer(window, cx));
    }

    /// Asks before closing an editor with unsaved changes.
    fn confirm_discard(
        &mut self,
        canvas: Entity<ProjectCanvas>,
        item: u64,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let alert = cx.new(|cx| {
            AlertDialog::destructive(
                format!("Discard changes to “{name}”?"),
                "Your changes to this file haven't been saved.",
                "Discard changes",
                window,
                cx,
            )
        });
        let subscription = cx.subscribe(&alert, move |this, alert, event, cx| {
            if let AlertDialogEvent::Confirm = event {
                canvas.update(cx, |canvas, cx| canvas.discard_item(item, cx));
            }
            this.close_modal(alert.entity_id(), cx);
        });
        self.modal = Some(Modal {
            view: alert.into(),
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Files open with unsaved changes, in every project.
    fn unsaved_files(&self, cx: &App) -> Vec<String> {
        self.canvases
            .values()
            .flat_map(|(canvas, _)| canvas.read(cx).unsaved_files(cx))
            .collect()
    }

    /// Quits, asking first if files have unsaved changes. Returns whether
    /// it's quitting now.
    fn quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let unsaved = self.unsaved_files(cx);
        if unsaved.is_empty() {
            cx.quit();
            return true;
        }
        if self.modal.is_some() {
            return false;
        }
        let description = match unsaved.as_slice() {
            [one] => format!("“{one}” has changes that haven't been saved."),
            many => format!("{} files have changes that haven't been saved: {}.", many.len(), many.join(", ")),
        };
        let alert = cx.new(|cx| AlertDialog::destructive("Quit without saving?", description, "Quit anyway", window, cx));
        let subscription = cx.subscribe(&alert, |this, alert, event, cx| {
            if let AlertDialogEvent::Confirm = event {
                cx.quit();
            }
            this.close_modal(alert.entity_id(), cx);
        });
        self.modal = Some(Modal {
            view: alert.into(),
            _subscription: subscription,
        });
        cx.notify();
        false
    }

    fn toggle_expanded(&mut self, id: u64, cx: &mut Context<Self>) {
        let expanded = self.projects.read(cx).is_expanded(id);
        self.update_projects(cx, |store| store.set_expanded(id, !expanded));
    }

    fn toggle_pin(&mut self, id: u64, _: &mut Window, cx: &mut Context<Self>) {
        let pinned = self.project(id, cx).is_some_and(|p| p.pinned);
        self.update_projects(cx, |store| store.set_pinned(id, !pinned));
    }

    /// Turns the project's label into a text field, with the name selected.
    fn start_rename(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id, cx) else {
            return;
        };
        let name = project.name.clone();
        let input = cx.new(|cx| TextInput::new("Project name", cx).inline().with_text(name));
        let focus = input.focus_handle(cx);
        window.focus(&focus);
        // Moving focus elsewhere saves, like clicking away does.
        let blur = cx.on_blur(&focus, window, |this, _, cx| this.finish_rename(true, cx));
        self.rename = Some(Rename {
            project_id: id,
            input,
            _blur: blur,
        });
        cx.notify();
    }

    /// Ends an inline rename, saving the name if `save` and it isn't blank.
    fn finish_rename(&mut self, save: bool, cx: &mut Context<Self>) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let name = rename.input.read(cx).text().trim().to_string();
        if save && !name.is_empty() {
            self.update_projects(cx, |store| store.rename(rename.project_id, name));
        }
        cx.notify();
    }

    fn show_menu(
        &mut self,
        target: MenuTarget,
        entries: Vec<MenuEntry>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = cx.new(|cx| ContextMenu::new(entries, window, cx));
        let subscription = cx.subscribe(&menu, |this, menu, _: &DismissEvent, cx| {
            // A click that dismisses this menu may already have opened the
            // next one; only close the menu that asked.
            if this.menu.as_ref().is_some_and(|open| open.menu == menu) {
                this.menu = None;
                cx.notify();
            }
        });
        self.menu = Some(OpenMenu {
            menu,
            target,
            position,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn open_project_menu(
        &mut self,
        id: u64,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project(id, cx) else {
            return;
        };
        let pinned = project.pinned;

        // Each item calls back into the shell for this project.
        let shell = cx.entity().downgrade();
        let action = |f: fn(&mut Self, u64, &mut Window, &mut Context<Self>)| {
            let shell = shell.clone();
            move |window: &mut Window, cx: &mut App| {
                shell.update(cx, |this, cx| f(this, id, window, cx)).ok();
            }
        };
        let entries = vec![
            MenuEntry::item(
                IconName::SquareTerminal,
                "New terminal",
                action(Self::new_terminal),
            ),
            MenuEntry::item(IconName::FolderTree, "New explorer", action(Self::new_explorer)),
            MenuEntry::Separator,
            MenuEntry::item(IconName::Pencil, "Rename", action(Self::start_rename)),
            MenuEntry::item(
                IconName::Paintbrush,
                "Change icon & color…",
                action(Self::open_edit_dialog),
            ),
            if pinned {
                MenuEntry::item(IconName::PinOff, "Unpin", action(Self::toggle_pin))
            } else {
                MenuEntry::item(IconName::Pin, "Pin", action(Self::toggle_pin))
            },
            MenuEntry::Separator,
            MenuEntry::destructive(IconName::Trash, "Delete…", action(Self::confirm_delete)),
        ];
        self.show_menu(MenuTarget::Project(id), entries, position, window, cx);
    }

    fn open_item_menu(
        &mut self,
        project: u64,
        item: u64,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((open, kind, permanent)) = self
            .project(project, cx)
            .and_then(|p| Some((p.is_open(item), p.item(item)?.kind, p.is_permanent(item))))
        else {
            return;
        };
        let shell = cx.entity().downgrade();
        let action = |f: fn(&mut Self, u64, u64, &mut Window, &mut Context<Self>)| {
            let shell = shell.clone();
            move |window: &mut Window, cx: &mut App| {
                shell
                    .update(cx, |this, cx| f(this, project, item, window, cx))
                    .ok();
            }
        };
        let open_or_close = if open {
            MenuEntry::item(IconName::X, "Close window", action(Self::close_item))
        } else {
            MenuEntry::item(kind.icon(), "Open", action(Self::open_item))
        };
        if permanent {
            // Every project has its first Explorer and Lino: they open and
            // close, nothing more.
            self.show_menu(MenuTarget::Item(item), vec![open_or_close], position, window, cx);
            return;
        }
        if kind == ItemKind::Editor {
            // Closing an editor takes it out of the project.
            let entries = vec![
                MenuEntry::item(IconName::Pencil, "Rename…", action(Self::open_window_dialog)),
                MenuEntry::item(IconName::Paintbrush, "Change color…", action(Self::open_window_dialog)),
                MenuEntry::Separator,
                MenuEntry::item(IconName::X, "Close editor", action(Self::close_item)),
            ];
            self.show_menu(MenuTarget::Item(item), entries, position, window, cx);
            return;
        }
        let entries = vec![
            // Both open the window dialog, which has the name and color.
            MenuEntry::item(IconName::Pencil, "Rename…", action(Self::open_window_dialog)),
            MenuEntry::item(
                IconName::Paintbrush,
                "Change color…",
                action(Self::open_window_dialog),
            ),
            MenuEntry::Separator,
            open_or_close,
        ];
        self.show_menu(MenuTarget::Item(item), entries, position, window, cx);
    }

    fn close_item(&mut self, project: u64, item: u64, _: &mut Window, cx: &mut Context<Self>) {
        // Through the canvas (if it exists), which ends the window's session.
        match self.canvases.get(&project) {
            Some((canvas, _)) => canvas.update(cx, |canvas, cx| canvas.close_item(item, cx)),
            None => self.update_projects(cx, |store| store.close_item(project, item)),
        }
    }

    fn open_create_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| ProjectDialog::create(window, cx));
        self.show_project_dialog(dialog, cx);
    }

    fn open_edit_dialog(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id, cx).cloned() else {
            return;
        };
        let dialog = cx.new(|cx| ProjectDialog::edit_project(&project, window, cx));
        self.show_project_dialog(dialog, cx);
    }

    fn open_window_dialog(&mut self, project: u64, item: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.project(project, cx).and_then(|p| p.item(item)).cloned() else {
            return;
        };
        let dialog = cx.new(|cx| ProjectDialog::edit_window(project, &item, window, cx));
        self.show_project_dialog(dialog, cx);
    }

    fn show_project_dialog(&mut self, dialog: Entity<ProjectDialog>, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(&dialog, |this, dialog, event, cx| {
            if let ProjectDialogEvent::Save { target, draft } = event {
                let (name, icon, color) = (draft.name.clone(), draft.icon, draft.color);
                let folder = draft.folder.clone();
                let target = *target;
                // lino comes on for a project's folder when the project is
                // added (or moved to another folder).
                let new_folder = match target {
                    DialogTarget::NewProject => folder.clone(),
                    DialogTarget::Project(id) => folder
                        .clone()
                        .filter(|f| this.project(id, cx).and_then(|p| p.folder.as_ref()) != Some(f)),
                    DialogTarget::Window { .. } => None,
                };
                let remote = draft.remote.clone();
                if let Some(folder) = new_folder.filter(|_| Mods::installed(ItemKind::Lino, cx)) {
                    let server = match target {
                        DialogTarget::NewProject => remote.clone().map(server::HostAddress::Remote),
                        DialogTarget::Project(id) => this.project(id, cx).and_then(Project::server),
                        DialogTarget::Window { .. } => None,
                    };
                    enable_lino(folder, server, cx);
                }
                this.update_projects(cx, |store| match target {
                    DialogTarget::NewProject => store.create(name, icon, color, folder, remote),
                    DialogTarget::Project(id) => store.update(id, name, icon, color, folder),
                    DialogTarget::Window { project, item } => {
                        store.update_item(project, item, name, color)
                    }
                });
            }
            this.close_modal(dialog.entity_id(), cx);
        });
        self.modal = Some(Modal {
            view: dialog.into(),
            _subscription: subscription,
        });
        cx.notify();
    }

    fn confirm_delete(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.project(id, cx) else {
            return;
        };
        let title = format!("Delete “{}”?", project.name);
        let terminals = project.items.iter().filter(|i| i.kind == ItemKind::Terminal).count();
        let running = match terminals {
            0 => String::new(),
            1 => " and ends anything running in its window".to_string(),
            n => format!(" and ends anything running in its {n} windows"),
        };
        let description = match &project.folder {
            Some(folder) => format!(
                "This removes the project from xharness{running}. Its .xharness folder stays in {}, so adding that folder again brings the layout back.",
                ui::display_path(folder)
            ),
            None => format!("This removes the project from xharness{running}. It can't be undone."),
        };
        let alert = cx.new(|cx| {
            AlertDialog::destructive(title, description, "Delete project", window, cx)
        });
        let subscription = cx.subscribe(&alert, move |this, alert, event, cx| {
            if let AlertDialogEvent::Confirm = event {
                // End the project's shells. Dropping views only detaches,
                // and windows that were never opened have no view at all.
                if let Some((canvas, _)) = this.canvases.remove(&id) {
                    canvas.read(cx).kill_all(cx);
                }
                let sessions: Vec<String> = this
                    .project(id, cx)
                    .map(|p| p.items.iter().map(|i| i.session.clone()).collect())
                    .unwrap_or_default();
                let host = server::HostAddress::default();
                for session in sessions {
                    server::kill(&host, &session);
                }
                this.update_projects(cx, |store| store.delete(id));
                if this.rename.as_ref().is_some_and(|r| r.project_id == id) {
                    this.rename = None;
                }
                if this.selection == Selection::Project(id) {
                    this.selection = Selection::Page(Page::Home);
                }
            }
            this.close_modal(alert.entity_id(), cx);
        });
        self.modal = Some(Modal {
            view: alert.into(),
            _subscription: subscription,
        });
        cx.notify();
    }

    fn close_modal(&mut self, view: EntityId, cx: &mut Context<Self>) {
        if self.modal.as_ref().is_some_and(|m| m.view.entity_id() == view) {
            self.modal = None;
            cx.notify();
        }
    }

    // Rendering --------------------------------------------------------------
    /// SidebarInset: the rounded panel, the only raised surface. No header:
    /// the canvas (or Home) fills it.
    /// flex-1 bg-card text-card-foreground border rounded-2xl overflow-hidden
    fn render_inset(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let project = match self.selection {
            Selection::Project(id) => self.project(id, cx),
            Selection::Page(_) => None,
        };
        let canvas = project
            .and_then(|p| self.canvases.get(&p.id))
            .map(|(canvas, _)| canvas.clone());

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .rounded(radius::XL2)
            .border_1()
            .border_color(theme.border)
            .bg(theme.card)
            .text_color(theme.card_foreground)
            .child(match (self.selection, canvas) {
                // A project only opens while its server answers.
                (Selection::Project(_), _)
                    if let Some(project) = project
                        && let status = self.servers.read(cx).status(project.remote.as_ref())
                        && !status.is_connected() =>
                {
                    self.render_offline(project, &status, theme, cx)
                }
                (Selection::Project(_), Some(canvas)) => div()
                    .flex_1()
                    .min_h_0()
                    .child(canvas)
                    .into_any_element(),
                (Selection::Page(Page::Marketplace), _) => div()
                    .flex_1()
                    .min_h_0()
                    .child(self.marketplace.clone())
                    .into_any_element(),
                _ => self.render_home(theme, cx),
            })
    }

}


impl Render for AppShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            // ⌘Q and the menu's Quit: ask first about unsaved files.
            .on_action(cx.listener(|this, _: &Quit, window, cx| {
                this.quit(window, cx);
            }))
            .child(
                // The title bar, with the traffic lights at its left. macOS
                // drags the window from it; double-click zooms. Elsewhere it's
                // a drag area with our own caption buttons, which the system
                // handles (so Windows' snap layouts work on Maximize).
                div()
                    .id("titlebar")
                    .relative()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .h(px(TITLEBAR_HEIGHT))
                    .bg(gpui::rgb(TITLEBAR_COLOR))
                    .text_size(px(13.))
                    .line_height(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(gpui::white())
                    .map(|bar| {
                        if cfg!(target_os = "macos") {
                            bar.on_click(titlebar_double_click)
                        } else {
                            bar.window_control_area(WindowControlArea::Drag)
                        }
                    })
                    .child("XHarness")
                    .when(!cfg!(target_os = "macos"), |bar| bar.child(caption_buttons())),
            )
            .child(
                // SidebarProvider (variant "app"): p-2 gap-2 on the sidebar
                // color.
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(GUTTER))
                    .p(px(GUTTER))
                    .child(self.render_sidebar(theme, cx))
                    .child(self.render_inset(theme, cx)),
            )
            .child(self.render_footer(theme, cx))
            .when_some(self.modal.as_ref(), |el, modal| el.child(modal.view.clone()))
            // Menus paint above everything and open down-right from the
            // pointer, kept 8px inside the window.
            .when_some(self.menu.as_ref(), |el, open| {
                el.child(
                    deferred(
                        anchored()
                            .position(open.position)
                            .snap_to_window_with_margin(px(GUTTER))
                            .child(open.menu.clone()),
                    )
                    .with_priority(1),
                )
            })
    }
}

/// Double-clicking a titlebar area does what macOS is set to do (zoom,
/// minimize or nothing). Dragging is handled natively by macOS.
/// Sets lino up in `folder` and starts it, in the background. The Lino
/// window shows how it went.
fn enable_lino(folder: PathBuf, server: Option<server::HostAddress>, cx: &App) {
    cx.background_executor()
        .spawn(async move {
            if let Err(error) = crate::services::rpc::on(server.as_ref(), || lino::enable(&folder)) {
                eprintln!("xharness: can't set lino up in {}: {error}", folder.display());
            }
        })
        .detach();
}

/// Minimize, maximize and close, at the title bar's right (not on macOS,
/// which has its traffic lights).
fn caption_buttons() -> impl IntoElement {
    let button = |id: &'static str, icon: IconName, area: WindowControlArea, hover: gpui::Hsla| {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(46.))
            .h_full()
            .hover(move |style| style.bg(hover))
            .window_control_area(area)
            .child(ui::icon(icon).size_3p5().text_color(gpui::white()))
    };
    let lighter = gpui::white().opacity(0.12);
    div()
        .absolute()
        .top_0()
        .right_0()
        .h_full()
        .flex()
        .child(button("caption-minimize", IconName::Minus, WindowControlArea::Min, lighter))
        .child(button("caption-maximize", IconName::Square, WindowControlArea::Max, lighter))
        .child(button("caption-close", IconName::X, WindowControlArea::Close, gpui::rgb(0xc42b1c).into()))
}

fn titlebar_double_click(event: &ClickEvent, window: &mut Window, _: &mut App) {
    if event.click_count() == 2 {
        window.titlebar_double_click();
    }
}
