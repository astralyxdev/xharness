//! The dialog for a project (new or existing) or for a window.
//!
//! - New project: on this Mac (a folder picked with the system folder
//!   panel) or on a remote server (its key, pasted, and a folder there),
//!   then name, icon and color. Picking a folder names the project after it
//!   if no name was typed yet.
//! - Edit project: the same fields.
//! - Edit window: name and color only.
//!
//! Styled as the Astralyx `Dialog`. Like the site's, the backdrop is
//! transparent but modal: it blocks the app behind it, and clicking it or
//! pressing Escape dismisses. Enter saves.

use std::{path::PathBuf, time::Duration};

use gpui::{
    App, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    KeyBinding, PathPromptOptions, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::services::server::HostAddress;

use crate::ui::actions::{Cancel, Confirm};
use crate::ui::assets::IconName;
use crate::services::projects::{Project, ProjectColor, ProjectItem};
use crate::services::server::ServerKey;
use crate::ui::text_input::TextInput;
use crate::ui::theme::{Theme, radius};
use crate::ui::{self, Button, Variant, display_path};

const KEY_CONTEXT: &str = "ProjectDialog";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", Confirm, Some(KEY_CONTEXT)),
        KeyBinding::new("escape", Cancel, Some(KEY_CONTEXT)),
    ]);
}

/// What the dialog edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogTarget {
    NewProject,
    Project(u64),
    Window { project: u64, item: u64 },
}

/// What the dialog produces. `icon` and `folder` only matter for projects.
pub struct Draft {
    pub name: String,
    pub icon: IconName,
    pub color: ProjectColor,
    pub folder: Option<PathBuf>,
    /// A remote project's server.
    pub remote: Option<ServerKey>,
}

pub enum ProjectDialogEvent {
    Save { target: DialogTarget, draft: Draft },
    Dismiss,
}

pub struct ProjectDialog {
    target: DialogTarget,
    name: Entity<TextInput>,
    icon: IconName,
    color: ProjectColor,
    folder: Option<PathBuf>,
    /// The chosen folder already holds an xharness project with this many
    /// windows; creating opens it.
    existing: Option<usize>,
    /// New project on a remote server, rather than this Mac.
    remote: bool,
    key_input: Entity<TextInput>,
    remote_folder: Entity<TextInput>,
    /// The key and folder, checked on the server.
    check: Check,
    check_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Where a remote project's key and folder stand.
#[derive(Clone)]
enum Check {
    /// Nothing to check yet.
    Idle,
    Checking,
    /// Connected, and the folder's there: what to say, and the folder
    /// (with `~` resolved).
    Ready { summary: String, key: ServerKey, folder: PathBuf },
    Failed(String),
}

impl EventEmitter<ProjectDialogEvent> for ProjectDialog {}

impl ProjectDialog {
    pub fn create(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new("My project", cx));
        let draft = Draft {
            name: String::new(),
            icon: IconName::Folder,
            color: ProjectColor::Blue,
            folder: None,
            remote: None,
        };
        Self::new(DialogTarget::NewProject, name, draft, window, cx)
    }

    pub fn edit_project(project: &Project, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = project.name.clone();
        let name = cx.new(|cx| TextInput::new("My project", cx).with_text(text));
        let draft = Draft {
            name: String::new(),
            icon: project.icon,
            color: project.color,
            folder: project.folder.clone(),
            remote: project.remote.clone(),
        };
        Self::new(DialogTarget::Project(project.id), name, draft, window, cx)
    }

    pub fn edit_window(
        project: u64,
        item: &ProjectItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = item.title.clone();
        let name = cx.new(|cx| TextInput::new("Terminal", cx).with_text(text));
        let draft = Draft {
            name: String::new(),
            icon: item.kind.icon(),
            color: item.color,
            folder: None,
            remote: None,
        };
        let target = DialogTarget::Window {
            project,
            item: item.id,
        };
        Self::new(target, name, draft, window, cx)
    }

    fn new(
        target: DialogTarget,
        name: Entity<TextInput>,
        draft: Draft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let key_input = cx.new(|cx| TextInput::new("xhk1_…", cx));
        let remote_folder = cx.new(|cx| TextInput::new("~/projects/app", cx));
        // Re-render as the name changes: it enables the save button. The key
        // and the remote folder are checked as they change.
        let subscriptions = vec![
            cx.observe(&name, |_, _, cx| cx.notify()),
            cx.observe(&key_input, |this, _, cx| this.schedule_check(cx)),
            cx.observe(&remote_folder, |this, _, cx| this.schedule_check(cx)),
        ];
        window.focus(&name.focus_handle(cx));
        Self {
            target,
            name,
            icon: draft.icon,
            color: draft.color,
            folder: draft.folder,
            existing: None,
            remote: false,
            key_input,
            remote_folder,
            check: Check::Idle,
            check_task: None,
            _subscriptions: subscriptions,
        }
    }

    /// Checks the pasted key and folder on the server, a moment after typing
    /// stops.
    fn schedule_check(&mut self, cx: &mut Context<Self>) {
        let key_text = self.key_input.read(cx).text().trim().to_string();
        let folder_text = self.remote_folder.read(cx).text().trim().to_string();
        if key_text.is_empty() {
            self.check = Check::Idle;
            self.check_task = None;
            cx.notify();
            return;
        }
        let key = match ServerKey::decode(&key_text) {
            Ok(key) => key,
            Err(error) => {
                self.check = Check::Failed(error);
                self.check_task = None;
                cx.notify();
                return;
            }
        };
        if folder_text.is_empty() {
            self.check = Check::Failed(format!("Key for {} ({}). Now the project's folder there.", key.name, key.address()));
            self.check_task = None;
            cx.notify();
            return;
        }
        self.check = Check::Checking;
        cx.notify();
        let probe = key.clone();
        self.check_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let found = cx
                .background_executor()
                .spawn(async move {
                    let key = probe;
                    let server = HostAddress::Remote(key.clone());
                    crate::services::rpc::on(Some(&server), || {
                        let (latency, info) = crate::services::rpc::ping(Some(&server))?;
                        let folder = match folder_text.strip_prefix("~") {
                            Some(rest) => crate::services::files::home()
                                .ok_or("The server has no home folder.")?
                                .join(rest.trim_start_matches('/')),
                            None => PathBuf::from(&folder_text),
                        };
                        if crate::services::files::stamp(&folder).is_none() {
                            return Err(format!("{} has no folder {}.", key.name, folder.display()));
                        }
                        let existing = crate::services::projects::existing_project(&folder);
                        let summary = format!(
                            "Connected to {} ({}) · {} ms",
                            info.hostname,
                            info.os,
                            latency.as_millis().max(1)
                        );
                        Ok((summary, folder, existing))
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.check = match found {
                    Ok((summary, folder, existing)) => {
                        if let Some(existing) = &existing {
                            let name = existing.name.clone();
                            this.name.update(cx, |input, cx| input.set_text(name, cx));
                            this.icon = existing.icon;
                            this.color = existing.color;
                        } else if this.name(cx).is_empty()
                            && let Some(name) = folder.file_name()
                        {
                            let name = name.to_string_lossy().to_string();
                            this.name.update(cx, |input, cx| input.set_text(name, cx));
                        }
                        this.existing = existing.map(|e| e.windows);
                        Check::Ready { summary, key, folder }
                    }
                    Err(error) => Check::Failed(error),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn is_window(&self) -> bool {
        matches!(self.target, DialogTarget::Window { .. })
    }

    fn name(&self, cx: &App) -> String {
        self.name.read(cx).text().trim().to_string()
    }

    fn name_focus(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }

    /// A name, and for a new project a folder too (on a remote project, one
    /// the server has).
    fn can_save(&self, cx: &App) -> bool {
        if self.name(cx).is_empty() {
            return false;
        }
        match (self.target, self.remote) {
            (DialogTarget::NewProject, true) => matches!(self.check, Check::Ready { .. }),
            (DialogTarget::NewProject, false) => self.folder.is_some(),
            _ => true,
        }
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        if !self.can_save(cx) {
            return;
        }
        let (folder, remote) = match (&self.check, self.remote) {
            (Check::Ready { key, folder, .. }, true) => (Some(folder.clone()), Some(key.clone())),
            _ => (self.folder.clone(), None),
        };
        cx.emit(ProjectDialogEvent::Save {
            target: self.target,
            draft: Draft {
                name: self.name(cx),
                icon: self.icon,
                color: self.color,
                folder,
                remote,
            },
        });
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ProjectDialogEvent::Dismiss);
    }

    /// Opens the system folder panel.
    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            // Cancelled, or the panel failed: keep what we had.
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                let existing = (this.target == DialogTarget::NewProject)
                    .then(|| crate::services::projects::existing_project(&folder))
                    .flatten();
                if let Some(existing) = &existing {
                    // A folder that's been an xharness project before: open
                    // it as it was.
                    let name = existing.name.clone();
                    this.name.update(cx, |input, cx| input.set_text(name, cx));
                    this.icon = existing.icon;
                    this.color = existing.color;
                } else if this.name(cx).is_empty()
                    && let Some(name) = folder.file_name()
                {
                    // A project is usually named after its folder.
                    let name = name.to_string_lossy().to_string();
                    this.name.update(cx, |input, cx| input.set_text(name, cx));
                }
                this.existing = existing.map(|e| e.windows);
                this.folder = Some(folder);
                window.focus(&this.name_focus(cx));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// This Mac or a remote server, for a new project.
    fn render_location(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let option = |id: &'static str, icon: IconName, label: &'static str, remote: bool| {
            let selected = self.remote == remote;
            let hover = theme.accent;
            div()
                .id(id)
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .h_8()
                .rounded(radius::MD)
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .map(|el| if selected { el.bg(theme.background).text_color(theme.foreground) } else { el.text_color(theme.muted_foreground).hover(move |s| s.bg(hover)) })
                .child(ui::icon(icon).size_3p5())
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.remote = remote;
                    this.existing = None;
                    let focus = if remote { this.key_input.focus_handle(cx) } else { this.name_focus(cx) };
                    window.focus(&focus);
                    cx.notify();
                }))
        };
        div()
            .flex()
            .gap_1()
            .p_1()
            .rounded(radius::LG)
            .bg(theme.secondary)
            .child(option("location-local", IconName::HardDrive, "This Mac", false))
            .child(option("location-remote", IconName::Globe, "Remote server", true))
    }

    fn render_remote(&self, theme: &Theme) -> impl IntoElement {
        let (text, color) = match &self.check {
            Check::Idle => (
                "On the server, run `xharness --server --listen 0.0.0.0:38888` and paste the key it prints.".to_string(),
                theme.muted_foreground,
            ),
            Check::Checking => ("Connecting…".to_string(), theme.muted_foreground),
            Check::Ready { summary, folder, .. } => (format!("{summary} · {}", folder.display()), theme.tints.green.soft_foreground),
            Check::Failed(error) => (error.clone(), theme.tints.destructive.soft_foreground),
        };
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(field("Server key", self.key_input.clone()))
            .child(field("Folder on the server", self.remote_folder.clone()))
            .child(div().mt(px(-8.)).text_size(px(12.)).line_height(px(16.)).text_color(color).child(text))
    }

    fn render_folder_picker(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let (path, chosen) = match &self.folder {
            Some(folder) => (display_path(folder), true),
            None => ("No folder chosen".to_string(), false),
        };
        let hover_border = theme.border_active;

        div()
            .flex()
            .gap_2()
            .child(
                // Looks like a field; clicking it opens the panel too.
                div()
                    .id("folder-field")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .h_9()
                    .px_3()
                    .rounded(radius::CONTROL_MD)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .hover(move |style| style.border_color(hover_border))
                    .child(ui::icon(IconName::FolderOpen).text_color(theme.muted_foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(if chosen {
                                theme.foreground
                            } else {
                                theme.muted_foreground.opacity(0.7)
                            })
                            .child(path),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.choose_folder(window, cx))),
            )
            .child(
                Button::new("choose-folder", "Choose…")
                    .variant(Variant::Soft)
                    .on_click(cx.listener(|this, _, window, cx| this.choose_folder(window, cx))),
            )
    }

    /// 9 per row: 9 × 36px + 8 × 8px gap fits the 400px body.
    fn render_icon_picker(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let color = self.color.resolve(&theme.tints, theme.foreground);
        let (muted, hover_bg) = (theme.muted_foreground, theme.accent);

        div()
            .flex()
            .flex_wrap()
            .gap_2()
            .children(IconName::PROJECT.into_iter().map(|icon| {
                let selected = icon == self.icon;
                div()
                    .id(ElementId::named_usize("icon", icon as usize))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size_9()
                    .rounded(radius::CONTROL_MD)
                    .border_1()
                    .map(|tile| {
                        if selected {
                            tile.border_color(theme.border_active).bg(theme.secondary)
                        } else {
                            tile.border_color(gpui::transparent_black())
                                .hover(move |style| style.bg(hover_bg))
                        }
                    })
                    .child(ui::icon(icon).text_color(if selected { color } else { muted }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.icon = icon;
                        window.focus(&this.name_focus(cx));
                        cx.notify();
                    }))
            }))
    }

    fn render_color_picker(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap_2()
            .children(ProjectColor::ALL.into_iter().map(|color| {
                let selected = color == self.color;
                let swatch = color.resolve(&theme.tints, theme.muted_foreground);
                // A 2px ring around a 20px dot when selected.
                div()
                    .id(ElementId::named_usize("color", color as usize))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size_7()
                    .rounded_full()
                    .border_2()
                    .border_color(if selected {
                        swatch
                    } else {
                        gpui::transparent_black()
                    })
                    .when(!selected, |ring| {
                        ring.hover(move |style| style.border_color(swatch.opacity(0.4)))
                    })
                    .child(div().size_5().rounded_full().bg(swatch))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.color = color;
                        window.focus(&this.name_focus(cx));
                        cx.notify();
                    }))
            }))
    }
}

/// label: text-sm leading-none font-medium, gap-2 above its control.
fn field(label: impl Into<SharedString>, control: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(14.))
                .line_height(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .child(label.into()),
        )
        .child(control)
}

impl Render for ProjectDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let can_save = self.can_save(cx);
        let (title, description, save_label) = match self.target {
            DialogTarget::NewProject => match self.existing {
                Some(windows) => (
                    "Open project",
                    if windows == 1 {
                        "This folder has an xharness project. Its window comes back as it was."
                    } else {
                        "This folder has an xharness project. Its windows come back as they were."
                    },
                    "Open project",
                ),
                None if self.remote => (
                    "New project",
                    "A folder on a remote server: everything in the project runs there.",
                    "Create project",
                ),
                None => (
                    "New project",
                    "Choose the project's folder. It's saved there, in .xharness, and its terminals start there.",
                    "Create project",
                ),
            },
            DialogTarget::Project(_) => (
                "Edit project",
                "Change the name, folder, icon or color.",
                "Save",
            ),
            DialogTarget::Window { .. } => (
                "Edit window",
                "Change this window's name and color.",
                "Save",
            ),
        };
        let is_window = self.is_window();

        ui::modal_backdrop()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(
                ui::dialog_surface("project-dialog", theme)
                    .on_mouse_down_out(cx.listener(|_, _, _, cx| {
                        cx.emit(ProjectDialogEvent::Dismiss);
                    }))
                    // dialog-header: gap-1.5 p-6
                    .child(ui::dialog_header(title, description, px(6.), theme))
                    .child(
                        // dialog-body: px-6 pb-6
                        div()
                            .flex()
                            .flex_col()
                            .gap_5()
                            .px_6()
                            .pb_6()
                            .when(self.target == DialogTarget::NewProject, |body| {
                                body.child(self.render_location(theme, cx))
                            })
                            .when(!is_window && !self.remote, |body| {
                                body.child(field("Folder", self.render_folder_picker(theme, cx)))
                            })
                            .when(!is_window && self.remote, |body| body.child(self.render_remote(theme)))
                            .child(field("Name", self.name.clone()))
                            .when(!is_window, |body| {
                                body.child(field("Icon", self.render_icon_picker(theme, cx)))
                            })
                            .child(field("Color", self.render_color_picker(theme, cx))),
                    )
                    .child(
                        ui::dialog_footer(theme)
                            .child(
                                Button::new("cancel", "Cancel")
                                    .variant(Variant::Soft)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel(&Cancel, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("save", save_label)
                                    .disabled(!can_save)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm(&Confirm, window, cx)
                                    })),
                            ),
                    ),
            )
    }
}
