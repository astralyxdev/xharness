//! The Git window, laid out like GitHub Desktop.
//!
//! - The toolbar has the branch (click it to switch, or start a new one),
//!   the remote (click it for Remotes & GitHub: see `remotes.rs`), and the
//!   one sync action that fits: Publish, Pull, Push, or Fetch.
//! - Changes lists changed files with a checkbox each (unchecked files stay
//!   out of the commit), shows the selected file's diff, and has the commit
//!   message under the list (⌘Enter commits).
//! - History lists commits on every branch, with the branch graph's lines
//!   and badges for branches and tags; a commit shows its files and diffs.
//!
//! Status is read again every few seconds; history when HEAD or the
//! upstream moves. Everything git does runs in the background.

use std::{
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, KeyBinding, MouseButton, MouseDownEvent, PathBuilder, Pixels, Point, SharedString, Stateful, Subscription,
    Task, UniformListScrollHandle, Window, actions, canvas, div, fill, point, prelude::*, px, rems, size, uniform_list,
};

use crate::ui::assets::IconName;
use crate::ui::context_menu::MenuEntry;
use crate::windows::explorer::open_with_default_app;
use crate::services::git::{self, Branch, Change, ChangeKind, Commit, Diff, DiffLine, GraphRow, Status};
use crate::services::server::HostAddress;
use crate::ui::text_input::TextInput;
use crate::ui::theme::{FONT_MONO, Theme};
use crate::ui;

mod remotes;

use remotes::{RemoteForm, RemotesPanel};

actions!(git_view, [CommitChanges]);

const KEY_CONTEXT: &str = "Git";
const POLL_EVERY: Duration = Duration::from_millis(2500);
/// History rows, and each graph lane, in rems.
const HISTORY_ROW: f32 = 2.75;
const LANE: f32 = 0.875;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("secondary-enter", CommitChanges, Some(KEY_CONTEXT))]);
}

pub enum GitEvent {
    Menu { entries: Vec<MenuEntry>, position: Point<Pixels> },
    OpenFile(PathBuf),
    /// Ask before doing something that can't be undone.
    Confirm {
        title: String,
        description: String,
        confirm: String,
        action: Rc<dyn Fn(&mut App)>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Changes,
    History,
}

enum Repo {
    Checking,
    /// The folder isn't in a repository.
    None,
    Ready(PathBuf),
}

/// What a background look at the repository found.
struct Snapshot {
    repo: Option<PathBuf>,
    status: Result<Status, String>,
    branches: Vec<Branch>,
    remotes: Vec<String>,
    /// New history, when HEAD or the upstream moved.
    history: Option<(String, Vec<Commit>)>,
    /// The selected change's diff, read again.
    diff: Option<(String, Result<Diff, String>)>,
    /// Whether branches and remotes were read (they're kept otherwise).
    refs_read: bool,
}

/// What history depends on: when it changes, history is read again.
fn history_key(status: &Status) -> String {
    format!("{:?} {:?} {:?} {} {}", status.head, status.branch, status.upstream, status.ahead, status.behind)
}

fn look(folder: &Path, repo: Option<PathBuf>, known_history: Option<String>, selected: Option<Change>) -> Snapshot {
    // The repository is found once; after that only status runs every poll
    // (and the selected file's diff). Branches and remotes follow HEAD.
    let Some(repo) = repo.or_else(|| git::discover(folder)) else {
        return Snapshot {
            repo: None,
            status: Ok(Status::default()),
            branches: Vec::new(),
            remotes: Vec::new(),
            history: None,
            diff: None,
            refs_read: true,
        };
    };
    let status = git::status(&repo);
    let history = status.as_ref().ok().and_then(|status| {
        let key = history_key(status);
        (known_history.as_ref() != Some(&key)).then(|| (key, git::history(&repo).unwrap_or_default()))
    });
    let has_head = status.as_ref().is_ok_and(|s| s.head.is_some());
    let diff = selected
        .filter(|change| status.as_ref().is_ok_and(|s| s.changes.contains(change)))
        .map(|change| (change.path.clone(), git::diff_change(&repo, &change, has_head)));
    let moved = history.is_some();
    Snapshot {
        branches: if moved { git::branches(&repo).unwrap_or_default() } else { Vec::new() },
        remotes: if moved { git::remotes(&repo) } else { Vec::new() },
        refs_read: moved,
        repo: Some(repo),
        status,
        history,
        diff,
    }
}

pub struct GitView {
    folder: PathBuf,
    /// The server the repository is on (`None`: this machine).
    server: Option<HostAddress>,
    repo: Repo,
    tab: Tab,
    status: Status,
    /// Files left out of the next commit (by path).
    excluded: HashSet<String>,
    selected_change: Option<String>,
    history: Vec<Commit>,
    graph: Vec<GraphRow>,
    history_key: Option<String>,
    selected_commit: Option<usize>,
    commit_files: Vec<Change>,
    selected_commit_file: Option<String>,
    /// The diff on the right, and what it's of.
    diff: Option<(String, Result<Diff, String>)>,
    branches: Vec<Branch>,
    /// The remotes' names.
    remotes: Vec<String>,
    summary: Entity<TextInput>,
    description: Entity<TextInput>,
    branch_input: Entity<TextInput>,
    creating_branch: bool,
    /// Remotes & GitHub, while open.
    remotes_panel: Option<RemotesPanel>,
    remote_name: Entity<TextInput>,
    remote_url: Entity<TextInput>,
    token_input: Entity<TextInput>,
    repo_name: Entity<TextInput>,
    repo_description: Entity<TextInput>,
    busy: Option<&'static str>,
    error: Option<String>,
    focus_handle: FocusHandle,
    changes_scroll: UniformListScrollHandle,
    history_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    action: Option<Task<()>>,
    detail_task: Option<Task<()>>,
    _poll: Task<()>,
    _inputs: Vec<Subscription>,
}

impl EventEmitter<GitEvent> for GitView {}

impl GitView {
    pub fn new(folder: PathBuf, server: Option<HostAddress>, cx: &mut Context<Self>) -> Self {
        let summary = cx.new(|cx| TextInput::new("Summary (required)", cx).inline());
        let description = cx.new(|cx| TextInput::new("Description", cx).inline());
        let branch_input = cx.new(|cx| TextInput::new("New branch name", cx).inline());
        let remote_name = cx.new(|cx| TextInput::new("origin", cx).inline());
        let remote_url = cx.new(|cx| TextInput::new("https://github.com/you/repository.git", cx).inline());
        let token_input = cx.new(|cx| TextInput::new("ghp_… or github_pat_…", cx).inline().masked());
        let repo_name = cx.new(|cx| TextInput::new("Repository name", cx).inline());
        let repo_description = cx.new(|cx| TextInput::new("Optional", cx).inline());
        // Buttons follow what's typed.
        let inputs = [&summary, &branch_input, &remote_name, &remote_url, &token_input, &repo_name]
            .into_iter()
            .map(|input| cx.observe(input, |_, _, cx| cx.notify()))
            .collect();
        let poll = cx.spawn(async move |this, cx| {
            // While nothing changes, look less often (up to 4× the interval).
            let mut quiet = 0u32;
            loop {
                let Ok((folder, repo, known, selected, before, server)) = this.read_with(cx, |this, _| {
                    let state = (this.folder.clone(), this.root(), this.history_key.clone(), this.selected_change());
                    (state.0, state.1, state.2, state.3, this.status.clone(), this.server.clone())
                }) else {
                    break;
                };
                let snapshot = cx
                    .background_executor()
                    .spawn(async move { crate::services::rpc::on(server.as_ref(), || look(&folder, repo, known, selected)) })
                    .await;
                let changed = snapshot.history.is_some() || snapshot.status.as_ref().is_ok_and(|s| *s != before);
                quiet = if changed { 0 } else { (quiet + 1).min(12) };
                if this.update(cx, |this, cx| this.apply(snapshot, cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(POLL_EVERY * (1 + quiet / 4)).await;
            }
        });
        Self {
            folder,
            server,
            repo: Repo::Checking,
            tab: Tab::Changes,
            status: Status::default(),
            excluded: HashSet::new(),
            selected_change: None,
            history: Vec::new(),
            graph: Vec::new(),
            history_key: None,
            selected_commit: None,
            commit_files: Vec::new(),
            selected_commit_file: None,
            diff: None,
            branches: Vec::new(),
            remotes: Vec::new(),
            summary,
            description,
            branch_input,
            creating_branch: false,
            remotes_panel: None,
            remote_name,
            remote_url,
            token_input,
            repo_name,
            repo_description,
            busy: None,
            error: None,
            focus_handle: cx.focus_handle(),
            changes_scroll: UniformListScrollHandle::new(),
            history_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            action: None,
            detail_task: None,
            _poll: poll,
            _inputs: inputs,
        }
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// A few words for the window's header: the branch and changes.
    pub fn summary(&self) -> Option<String> {
        match &self.repo {
            Repo::Ready(_) => {
                let branch = self.status.branch.clone().unwrap_or_else(|| "detached HEAD".to_string());
                Some(match self.status.changes.len() {
                    0 => branch,
                    1 => format!("{branch} · 1 change"),
                    n => format!("{branch} · {n} changes"),
                })
            }
            Repo::None => Some("not a repository".to_string()),
            Repo::Checking => None,
        }
    }

    fn root(&self) -> Option<PathBuf> {
        match &self.repo {
            Repo::Ready(root) => Some(root.clone()),
            _ => None,
        }
    }

    fn selected_change(&self) -> Option<Change> {
        let path = self.selected_change.as_ref()?;
        self.status.changes.iter().find(|c| &c.path == path).cloned()
    }

    fn included(&self) -> Vec<&Change> {
        self.status.changes.iter().filter(|c| !self.excluded.contains(&c.path)).collect()
    }

    fn apply(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        let Some(repo) = snapshot.repo else {
            self.repo = Repo::None;
            cx.notify();
            return;
        };
        self.repo = Repo::Ready(repo);
        match snapshot.status {
            Ok(status) => {
                // Forget checkboxes of files that aren't changed any more.
                self.excluded.retain(|path| status.changes.iter().any(|c| &c.path == path));
                if self.selected_change.as_ref().is_none_or(|path| !status.changes.iter().any(|c| &c.path == path)) {
                    self.selected_change = status.changes.first().map(|c| c.path.clone());
                    if self.tab == Tab::Changes {
                        self.diff = None;
                        self.load_selected_diff(cx);
                    }
                }
                self.status = status;
            }
            Err(error) => self.error = Some(error),
        }
        if snapshot.refs_read {
            self.branches = snapshot.branches;
            self.remotes = snapshot.remotes;
        }
        if let Some((key, history)) = snapshot.history {
            let selected = self.selected_commit.and_then(|ix| self.history.get(ix)).map(|c| c.hash.clone());
            self.graph = git::graph(&history);
            self.history = history;
            self.history_key = Some(key);
            self.selected_commit = selected.and_then(|hash| self.history.iter().position(|c| c.hash == hash));
        }
        if self.tab == Tab::Changes
            && let Some((path, diff)) = snapshot.diff
            && self.selected_change.as_ref() == Some(&path)
        {
            self.diff = Some((path, diff));
        }
        cx.notify();
    }

    /// Looks again now (after an action).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let folder = self.folder.clone();
        let repo = self.root();
        let selected = self.selected_change();
        let server = self.server.clone();
        self.history_key = None;
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let snapshot = cx
                .background_executor()
                .spawn(async move { crate::services::rpc::on(server.as_ref(), || look(&folder, repo, None, selected)) })
                .await;
            this.update(cx, |this, cx| this.apply(snapshot, cx)).ok();
        }));
    }

    /// Runs a git command in the background, then looks again.
    fn act(&mut self, busy: &'static str, action: impl FnOnce(&Path) -> Result<(), String> + Send + 'static, cx: &mut Context<Self>) {
        let Some(root) = self.root() else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(busy);
        self.error = None;
        cx.notify();
        let server = self.server.clone();
        self.action = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { crate::services::rpc::on(server.as_ref(), || action(&root)) }).await;
            this.update(cx, |this, cx| {
                this.busy = None;
                if let Err(error) = result {
                    this.error = Some(error);
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn load_selected_diff(&mut self, cx: &mut Context<Self>) {
        let (Some(root), Some(change)) = (self.root(), self.selected_change()) else {
            self.diff = None;
            return;
        };
        let has_head = self.status.head.is_some();
        let server = self.server.clone();
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let path = change.path.clone();
            let diff = cx
                .background_executor()
                .spawn(async move { crate::services::rpc::on(server.as_ref(), || git::diff_change(&root, &change, has_head)) })
                .await;
            this.update(cx, |this, cx| {
                if this.selected_change.as_ref() == Some(&path) {
                    this.diff = Some((path, diff));
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn select_change(&mut self, path: String, cx: &mut Context<Self>) {
        if self.selected_change.as_ref() != Some(&path) {
            self.selected_change = Some(path);
            self.diff_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
            self.load_selected_diff(cx);
        }
        cx.notify();
    }

    fn select_commit(&mut self, ix: usize, cx: &mut Context<Self>) {
        let (Some(root), Some(commit)) = (self.root(), self.history.get(ix).cloned()) else {
            return;
        };
        self.selected_commit = Some(ix);
        self.commit_files.clear();
        self.selected_commit_file = None;
        self.diff = None;
        cx.notify();
        let server = self.server.clone();
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let hash = commit.hash.clone();
            let (files, diff) = cx
                .background_executor()
                .spawn(async move {
                    crate::services::rpc::on(server.as_ref(), || {
                        let files = git::commit_files(&root, &commit.hash).unwrap_or_default();
                        let diff = files.first().map(|file| (file.path.clone(), git::diff_in_commit(&root, &commit.hash, file)));
                        (files, diff)
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                if this.selected_commit.and_then(|ix| this.history.get(ix)).is_some_and(|c| c.hash == hash) {
                    this.selected_commit_file = files.first().map(|f| f.path.clone());
                    this.commit_files = files;
                    this.diff = diff;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn select_commit_file(&mut self, path: String, cx: &mut Context<Self>) {
        let (Some(root), Some(commit)) = (self.root(), self.selected_commit.and_then(|ix| self.history.get(ix)).cloned())
        else {
            return;
        };
        let Some(file) = self.commit_files.iter().find(|f| f.path == path).cloned() else {
            return;
        };
        self.selected_commit_file = Some(path.clone());
        cx.notify();
        let server = self.server.clone();
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            let diff = cx
                .background_executor()
                .spawn(async move { crate::services::rpc::on(server.as_ref(), || git::diff_in_commit(&root, &commit.hash, &file)) })
                .await;
            this.update(cx, |this, cx| {
                if this.selected_commit_file.as_ref() == Some(&path) {
                    this.diff = Some((path, diff));
                    this.diff_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.diff = None;
        match tab {
            Tab::Changes => self.load_selected_diff(cx),
            Tab::History => {
                if let Some(ix) = self.selected_commit.or((!self.history.is_empty()).then_some(0)) {
                    self.select_commit(ix, cx);
                }
            }
        }
        cx.notify();
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        let summary = self.summary.read(cx).text().trim().to_string();
        let description = self.description.read(cx).text().to_string();
        let included = self.included();
        if summary.is_empty() || included.is_empty() || self.busy.is_some() {
            return;
        }
        // Everything, or just the checked files (renames with where they
        // came from).
        let paths: Option<Vec<String>> = (!self.excluded.is_empty()).then(|| {
            included
                .iter()
                .flat_map(|c| std::iter::once(c.path.clone()).chain(c.from.clone()))
                .collect()
        });
        self.summary.update(cx, |input, cx| input.set_text("", cx));
        self.description.update(cx, |input, cx| input.set_text("", cx));
        self.excluded.clear();
        self.act("Committing…", move |root| git::commit(root, &summary, &description, paths.as_deref()), cx);
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let status = self.status.clone();
        if status.upstream.is_none() {
            self.act("Publishing…", move |root| git::push(root, &status), cx);
        } else if status.behind > 0 {
            self.act("Pulling…", git::pull, cx);
        } else if status.ahead > 0 {
            self.act("Pushing…", move |root| git::push(root, &status), cx);
        } else {
            self.act("Fetching…", git::fetch, cx);
        }
    }

    fn branch_menu(&self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let mut entries: Vec<MenuEntry> = Vec::new();
        let (local, remote): (Vec<&Branch>, Vec<&Branch>) = self.branches.iter().partition(|b| !b.remote);
        for branch in local.into_iter().chain(remote.iter().copied().take(20)) {
            let this = this.clone();
            let target = branch.clone();
            entries.push(MenuEntry::check(branch.name.clone(), branch.current, move |_, cx| {
                let target = target.clone();
                this.update(cx, |this, cx| this.act("Switching…", move |root| git::switch(root, &target), cx))
                    .ok();
            }));
        }
        if !entries.is_empty() {
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::item(IconName::Plus, "New branch…", move |window, cx| {
            this.update(cx, |this, cx| {
                this.creating_branch = true;
                this.branch_input.update(cx, |input, cx| input.set_text("", cx));
                window.focus(&this.branch_input.focus_handle(cx));
                cx.notify();
            })
            .ok();
        }));
        cx.emit(GitEvent::Menu { entries, position });
    }

    fn create_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.branch_input.read(cx).text().trim().to_string();
        if name.is_empty() {
            return;
        }
        self.creating_branch = false;
        window.focus(&self.focus_handle);
        self.act("Creating branch…", move |root| git::create_branch(root, &name), cx);
    }

    fn change_menu(&self, change: &Change, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(root) = self.root() else {
            return;
        };
        let path = root.join(&change.path);
        let this = cx.entity().downgrade();
        let mut entries = Vec::new();
        if change.kind != ChangeKind::Deleted {
            let (this, path) = (this.clone(), path.clone());
            entries.push(MenuEntry::item(IconName::FileCode, "Open", move |_, cx| {
                let path = path.clone();
                this.update(cx, |_, cx| cx.emit(GitEvent::OpenFile(path))).ok();
            }));
            let path = root.join(&change.path);
            entries.push(MenuEntry::item(IconName::FolderOpen, "Reveal in Finder", move |_, _| {
                open_with_default_app(&path, true)
            }));
        }
        let relative = change.path.clone();
        entries.push(MenuEntry::item(IconName::Copy, "Copy path", move |_, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(relative.clone()));
        }));
        entries.push(MenuEntry::Separator);
        let target = change.clone();
        entries.push(MenuEntry::destructive(IconName::Trash, "Discard changes…", move |_, cx| {
            let (weak, change) = (this.clone(), target.clone());
            let action: Rc<dyn Fn(&mut App)> = Rc::new(move |cx: &mut App| {
                let target = change.clone();
                weak.update(cx, |this, cx| this.act("Discarding…", move |root| git::discard(root, &target), cx))
                    .ok();
            });
            let name = Path::new(&target.path).file_name().map_or(target.path.clone(), |n| n.to_string_lossy().into_owned());
            this.update(cx, |_, cx| {
                cx.emit(GitEvent::Confirm {
                    title: format!("Discard changes to “{name}”?"),
                    description: "The file goes back to how it was in the last commit. This can't be undone.".to_string(),
                    confirm: "Discard changes".to_string(),
                    action,
                })
            })
            .ok();
        }));
        cx.emit(GitEvent::Menu { entries, position });
    }

    // Rendering ------------------------------------------------------------------------

    fn render_toolbar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let branch = self.status.branch.clone().unwrap_or_else(|| "Detached HEAD".to_string());
        let bar = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .h_10()
            .px_2()
            .border_b_1()
            .border_color(theme.border);
        if self.creating_branch {
            let can_create = !self.branch_input.read(cx).text().trim().is_empty();
            return bar
                .child(
                    field(theme)
                        .flex_1()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.creating_branch = false;
                            cx.notify();
                        }))
                        .child(ui::icon(IconName::Plus).size_3p5().text_color(theme.muted_foreground))
                        .child(div().flex_1().min_w_0().child(self.branch_input.clone())),
                )
                .child(
                    ui::compact_button("git-create-branch", None, "Create branch", true, theme)
                        .when(!can_create, |b| b.opacity(0.5))
                        .on_click(cx.listener(|this, _, window, cx| this.create_branch(window, cx))),
                )
                .into_any_element();
        }
        let status = &self.status;
        let has_remote = !self.remotes.is_empty();
        let (icon, label) = match self.busy {
            Some(busy) => (IconName::RefreshCw, busy.to_string()),
            None if !has_remote => (IconName::Plus, "Add remote".to_string()),
            None if status.upstream.is_none() => (IconName::ArrowUp, "Publish branch".to_string()),
            None if status.behind > 0 => (IconName::ArrowDown, format!("Pull {}", status.behind)),
            None if status.ahead > 0 => (IconName::ArrowUp, format!("Push {}", status.ahead)),
            None => (IconName::RefreshCw, "Fetch".to_string()),
        };
        let can_sync = self.busy.is_none() && has_remote && status.head.is_some();
        let remote_label = match self.remotes.as_slice() {
            [] => None,
            [only] => Some(only.clone()),
            [first, rest @ ..] => Some(format!("{first} +{}", rest.len())),
        };
        bar.child(
            div()
                .id("git-branch")
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_2()
                .h_8()
                .px_2()
                .rounded(rems(0.5))
                .cursor_pointer()
                .hover(|style| style.bg(theme.secondary))
                .child(ui::icon(IconName::GitBranch).size_4().text_color(theme.muted_foreground))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(rems(0.625))
                                .line_height(rems(0.75))
                                .text_color(theme.muted_foreground)
                                .child("Current branch"),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(rems(0.8125))
                                .line_height(rems(1.125))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(branch),
                        ),
                )
                .child(div().flex_1())
                .child(ui::chevron(false, theme).size_3())
                .on_click(cx.listener(|this, event: &ClickEvent, _, cx| this.branch_menu(event.position(), cx))),
        )
        .children(remote_label.map(|label| {
            ui::compact_button("git-remotes", Some(IconName::Cloud), &label, false, theme)
                .when(self.remotes_open(), |b| b.bg(theme.secondary_active))
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.remotes_open() {
                        this.close_remotes(window, cx);
                    } else {
                        this.open_remotes(RemoteForm::None, window, cx);
                    }
                }))
        }))
        .child(
            ui::compact_button("git-sync", Some(icon), &label, !has_remote && self.busy.is_none(), theme)
                .when(has_remote && !can_sync, |b| b.opacity(0.5))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if !has_remote {
                        this.open_remotes(RemoteForm::Add, window, cx);
                    } else if can_sync {
                        this.sync(cx);
                    }
                })),
        )
        .into_any_element()
    }

    fn render_tabs(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label: String, tab: Tab| {
            let active = self.tab == tab;
            div()
                .id(id)
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .h_8()
                .text_size(rems(0.75))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .border_b_2()
                .border_color(if active { theme.tints.blue.base } else { gpui::transparent_black() })
                .text_color(if active { theme.foreground } else { theme.muted_foreground })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
        };
        let changes = match self.status.changes.len() {
            0 => "Changes".to_string(),
            n => format!("Changes {n}"),
        };
        div()
            .flex()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(tab("git-tab-changes", changes, Tab::Changes))
            .child(tab("git-tab-history", "History".to_string(), Tab::History))
    }

    fn render_changes(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let changes = self.status.changes.len();
        let included = self.included().len();
        if changes == 0 {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_center()
                .text_size(rems(0.75))
                .text_color(theme.muted_foreground)
                .child("No changes. Edits to files in this repository show up here.")
                .into_any_element();
        }
        let all = included == changes;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("git-include-all")
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .h_8()
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(rems(0.75))
                    .text_color(theme.muted_foreground)
                    .cursor_pointer()
                    .child(checkbox(all, included > 0 && !all, theme))
                    .child(format!("{changes} changed {}", if changes == 1 { "file" } else { "files" }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if all {
                            this.excluded = this.status.changes.iter().map(|c| c.path.clone()).collect();
                        } else {
                            this.excluded.clear();
                        }
                        cx.notify();
                    })),
            )
            .child(
                uniform_list(
                    "git-changes",
                    changes,
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        let theme = Theme::global(cx).clone();
                        range
                            .filter_map(|ix| this.status.changes.get(ix).cloned().map(|change| (ix, change)))
                            .map(|(ix, change)| this.render_change(ix, &change, &theme, cx).into_any_element())
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .track_scroll(self.changes_scroll.clone()),
            )
            .into_any_element()
    }

    fn render_change(&self, ix: usize, change: &Change, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.selected_change.as_ref() == Some(&change.path);
        let included = !self.excluded.contains(&change.path);
        let (dir, name) = match change.path.rsplit_once('/') {
            Some((dir, name)) => (format!("{dir}/"), name.to_string()),
            None => (String::new(), change.path.clone()),
        };
        let path = change.path.clone();
        let hover = theme.accent;
        div()
            .id(ElementId::named_usize("git-change", ix))
            .flex()
            .items_center()
            .gap_2()
            .h_7()
            .px_3()
            .text_size(rems(0.75))
            .map(|row| if selected { row.bg(theme.tints.blue.soft) } else { row.hover(move |s| s.bg(hover)) })
            .child(
                div()
                    .id(ElementId::named_usize("git-check", ix))
                    .child(checkbox(included, false, theme))
                    .on_click(cx.listener({
                        let path = path.clone();
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            if !this.excluded.remove(&path) {
                                this.excluded.insert(path.clone());
                            }
                            cx.notify();
                        }
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().flex_shrink_0().text_color(theme.muted_foreground).child(dir))
                    .child(div().min_w_0().truncate().child(name)),
            )
            .child(status_badge(change.kind, theme))
            .on_click(cx.listener({
                let path = path.clone();
                move |this, _, window, cx| {
                    window.focus(&this.focus_handle);
                    this.select_change(path.clone(), cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let change = change.clone();
                    move |this, event: &MouseDownEvent, _, cx| {
                        this.select_change(change.path.clone(), cx);
                        this.change_menu(&change, event.position, cx);
                    }
                }),
            )
    }

    fn render_commit_box(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let included = self.included().len();
        let ready = !self.summary.read(cx).text().trim().is_empty() && included > 0 && self.busy.is_none();
        let branch = self.status.branch.clone().unwrap_or_else(|| "HEAD".to_string());
        let label = match included {
            0 => "Nothing to commit".to_string(),
            1 => format!("Commit 1 file to {branch}"),
            n => format!("Commit {n} files to {branch}"),
        };
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .child(field(theme).child(div().flex_1().min_w_0().child(self.summary.clone())))
            .child(field(theme).child(div().flex_1().min_w_0().child(self.description.clone())))
            .child(
                ui::compact_button("git-commit", Some(IconName::Check), &label, true, theme)
                    .justify_center()
                    .when(!ready, |b| b.opacity(0.5))
                    .on_click(cx.listener(|this, _, _, cx| this.commit(cx))),
            )
    }

    fn render_history(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        if self.history.is_empty() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(rems(0.75))
                .text_color(theme.muted_foreground)
                .child("No commits yet.")
                .into_any_element();
        }
        uniform_list(
            "git-history",
            self.history.len(),
            cx.processor(|this, range: Range<usize>, _, cx| {
                let theme = Theme::global(cx).clone();
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                range.map(|ix| this.render_commit(ix, now, &theme, cx).into_any_element()).collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .track_scroll(self.history_scroll.clone())
        .into_any_element()
    }

    fn render_commit(&self, ix: usize, now: i64, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let commit = &self.history[ix];
        let row = self.graph.get(ix).cloned();
        let lanes = row.as_ref().map_or(1, |row| row.lanes.max(1));
        let selected = self.selected_commit == Some(ix);
        let palette = lane_palette(theme);
        let background = if selected { theme.tints.blue.soft } else { theme.background };
        let ago = format!("{} ago", crate::services::lino::format_span((now - commit.time).max(0) as u64).split(' ').next().unwrap_or(""));
        let hover = theme.accent;
        div()
            .id(ElementId::named_usize("git-commit-row", ix))
            .flex()
            .items_center()
            .h(rems(HISTORY_ROW))
            .pr_3()
            .map(|el| if selected { el.bg(background) } else { el.hover(move |s| s.bg(hover)) })
            .child(
                div().flex_shrink_0().h_full().w(rems(lanes as f32 * LANE + 0.5)).child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            if let Some(row) = &row {
                                paint_graph(bounds, row, &palette, background, window);
                            }
                        },
                    )
                    .size_full(),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .children(commit.refs.iter().take(3).map(|r| ref_badge(r, theme)))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(rems(0.8125))
                                    .line_height(rems(1.125))
                                    .child(commit.subject.clone()),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(rems(0.6875))
                            .line_height(rems(1.))
                            .text_color(theme.muted_foreground)
                            .child(format!("{} · {ago}", commit.author)),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.focus_handle);
                this.select_commit(ix, cx);
            }))
    }

    /// The right side: the diff, under the commit's details in History.
    fn render_detail(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let mut detail = div().flex().flex_col().flex_1().min_w_0().min_h_0();
        if self.tab == Tab::History
            && let Some(commit) = self.selected_commit.and_then(|ix| self.history.get(ix))
        {
            detail = detail.child(self.render_commit_header(commit, theme, cx));
        }
        detail.child(self.render_diff(theme, cx))
    }

    fn render_commit_header(&self, commit: &Commit, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let files = self.commit_files.len();
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .text_size(rems(0.875))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(commit.subject.clone()),
                    )
                    .when(!commit.body.is_empty(), |el| {
                        el.child(
                            div()
                                .max_h(rems(4.5))
                                .overflow_hidden()
                                .text_size(rems(0.75))
                                .text_color(theme.muted_foreground)
                                .child(commit.body.clone()),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .text_size(rems(0.6875))
                            .text_color(theme.muted_foreground)
                            .child(commit.author.clone())
                            .child(div().font_family(FONT_MONO).child(commit.short().to_string()))
                            .child(format!("{files} changed {}", if files == 1 { "file" } else { "files" })),
                    ),
            )
            .child(
                div()
                    .id("git-commit-files")
                    .flex()
                    .flex_col()
                    .max_h(rems(9.))
                    .overflow_y_scroll()
                    .border_t_1()
                    .border_color(theme.border)
                    .py_1()
                    .children(self.commit_files.iter().enumerate().map(|(ix, file)| {
                        let selected = self.selected_commit_file.as_ref() == Some(&file.path);
                        let path = file.path.clone();
                        let hover = theme.accent;
                        div()
                            .id(ElementId::named_usize("git-commit-file", ix))
                            .flex()
                            .items_center()
                            .gap_2()
                            .h_6()
                            .px_3()
                            .text_size(rems(0.75))
                            .map(|el| if selected { el.bg(theme.tints.blue.soft) } else { el.hover(move |s| s.bg(hover)) })
                            .child(div().flex_1().min_w_0().truncate().child(file.path.clone()))
                            .child(status_badge(file.kind, theme))
                            .on_click(cx.listener(move |this, _, _, cx| this.select_commit_file(path.clone(), cx)))
                    })),
            )
    }

    fn render_diff(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let message = |text: String| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_center()
                .text_size(rems(0.75))
                .text_color(theme.muted_foreground)
                .child(text)
                .into_any_element()
        };
        let Some((path, diff)) = &self.diff else {
            return message(String::new());
        };
        let diff = match diff {
            Ok(diff) => diff,
            Err(error) => return message(error.clone()),
        };
        if diff.lines.is_empty() {
            return message("No changes to show (only the file's mode, or nothing at all, changed).".to_string());
        }
        let lines = diff.lines.len();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .h_8()
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(rems(0.75))
                    .child(div().flex_1().min_w_0().truncate().child(path.clone()))
                    .child(div().text_color(theme.tints.green.soft_foreground).child(format!("+{}", diff.added)))
                    .child(div().text_color(theme.tints.destructive.soft_foreground).child(format!("−{}", diff.removed)))
                    .when(diff.truncated, |el| el.child(div().text_color(theme.muted_foreground).child("(cut short)"))),
            )
            .child(
                uniform_list(
                    "git-diff",
                    lines,
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        let theme = Theme::global(cx).clone();
                        let Some((_, Ok(diff))) = &this.diff else {
                            return Vec::new();
                        };
                        range
                            .filter_map(|ix| diff.lines.get(ix).map(|line| diff_row(line, &theme).into_any_element()))
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .track_scroll(self.diff_scroll.clone()),
            )
            .into_any_element()
    }
}

impl Render for GitView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let root = div()
            .id("git")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_size(rems(0.8125))
            .on_action(cx.listener(|this, _: &CommitChanges, _, cx| this.commit(cx)))
            .on_action(cx.listener(|this, _: &crate::ui::actions::Confirm, window, cx| {
                if this.creating_branch {
                    this.create_branch(window, cx);
                } else if this.remotes_open() {
                    this.remotes_confirm(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &crate::ui::actions::Cancel, window, cx| {
                if this.creating_branch {
                    this.creating_branch = false;
                    window.focus(&this.focus_handle);
                    cx.notify();
                } else if this.remotes_open() {
                    this.remotes_cancel(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            // Lists scroll; the canvas shouldn't pan along with them.
            .on_scroll_wheel(cx.listener(|this, _, window, cx| {
                if this.contains_focus(window, cx) {
                    cx.stop_propagation();
                }
            }));
        match &self.repo {
            Repo::Checking => root.child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.muted_foreground)
                    .child("Looking for a repository…"),
            ),
            Repo::None => root.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .px_8()
                    .text_center()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("This folder isn't a git repository"))
                    .child(
                        div()
                            .max_w(rems(26.))
                            .text_color(theme.muted_foreground)
                            .child(format!("Create one in {} to track changes and commit them.", ui::display_path(&self.folder))),
                    )
                    .child(
                        ui::compact_button("git-init", Some(IconName::Plus), "Create repository", true, theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                let folder = this.folder.clone();
                                let server = this.server.clone();
                                this.repo = Repo::Checking;
                                this.action = Some(cx.spawn(async move |this, cx| {
                                    let result = cx.background_executor().spawn(async move { crate::services::rpc::on(server.as_ref(), || git::init(&folder)) }).await;
                                    this.update(cx, |this, cx| {
                                        if let Err(error) = result {
                                            this.error = Some(error);
                                        }
                                        this.refresh(cx);
                                    })
                                    .ok();
                                }));
                                cx.notify();
                            }),
                        ),
                    ),
            ),
            Repo::Ready(_) => root
                .child(self.render_toolbar(theme, cx))
                .children(self.error.clone().map(|error| {
                    div()
                        .id("git-error")
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(theme.tints.destructive.base.opacity(0.08))
                        .text_size(rems(0.75))
                        .text_color(theme.tints.destructive.soft_foreground)
                        .child(div().flex_1().min_w_0().child(error))
                        .child(ui::small_icon_button("git-error-close", IconName::X, theme).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.error = None;
                                cx.notify();
                            },
                        )))
                }))
                .when(self.remotes_open(), |root| root.child(self.render_remotes(theme, cx)))
                .when(!self.remotes_open(), |root| root.child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_shrink_0()
                                .w(rems(19.))
                                .border_r_1()
                                .border_color(theme.border)
                                .child(self.render_tabs(theme, cx))
                                .map(|pane| match self.tab {
                                    Tab::Changes => pane
                                        .child(self.render_changes(theme, cx))
                                        .child(self.render_commit_box(theme, cx)),
                                    Tab::History => pane.child(self.render_history(theme, cx)),
                                }),
                        )
                        .child(self.render_detail(theme, cx)),
                )),
        }
    }
}

// Pieces ---------------------------------------------------------------------------

/// A small field: border bg-background h-8 rounded.
fn field(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .h_8()
        .px_2()
        .rounded(rems(0.5))
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .text_size(rems(0.75))
}

fn checkbox(checked: bool, partly: bool, theme: &Theme) -> Div {
    let on = checked || partly;
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(rems(0.875))
        .rounded(rems(0.1875))
        .border_1()
        .border_color(if on { theme.primary } else { theme.border })
        .when(on, |b| b.bg(theme.primary))
        .when(checked, |b| b.child(ui::icon(IconName::Check).size(rems(0.625)).text_color(theme.primary_foreground)))
        .when(partly && !checked, |b| b.child(div().w(rems(0.5)).h(px(1.5)).bg(theme.primary_foreground)))
}

fn status_badge(kind: ChangeKind, theme: &Theme) -> Div {
    let tint = match kind {
        ChangeKind::Added | ChangeKind::Untracked => theme.tints.green,
        ChangeKind::Modified => theme.tints.amber,
        ChangeKind::Deleted => theme.tints.destructive,
        ChangeKind::Renamed => theme.tints.blue,
        ChangeKind::Conflicted => theme.tints.rose,
    };
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(rems(1.))
        .rounded(rems(0.25))
        .bg(tint.base.opacity(0.15))
        .text_size(rems(0.625))
        .font_weight(FontWeight::BOLD)
        .text_color(tint.soft_foreground)
        .child(kind.letter())
}

/// A branch or tag on a commit: `HEAD -> main` (filled), `origin/main`
/// (outlined), `tag: v1` (amber).
fn ref_badge(name: &str, theme: &Theme) -> Div {
    let (label, bg, fg, border) = if let Some(branch) = name.strip_prefix("HEAD -> ") {
        (branch.to_string(), theme.tints.blue.base, theme.tints.blue.foreground, theme.tints.blue.base)
    } else if let Some(tag) = name.strip_prefix("tag: ") {
        (tag.to_string(), theme.tints.amber.base.opacity(0.15), theme.tints.amber.soft_foreground, theme.tints.amber.base.opacity(0.4))
    } else if name.contains('/') {
        (name.to_string(), gpui::transparent_black(), theme.muted_foreground, theme.border)
    } else {
        (name.to_string(), theme.tints.blue.base.opacity(0.15), theme.tints.blue.soft_foreground, theme.tints.blue.base.opacity(0.4))
    };
    div()
        .flex_shrink_0()
        .max_w(rems(9.))
        .truncate()
        .px_1p5()
        .rounded(rems(0.25))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(rems(0.625))
        .line_height(rems(0.875))
        .font_weight(FontWeight::MEDIUM)
        .child(label)
}

fn diff_row(line: &DiffLine, theme: &Theme) -> Stateful<Div> {
    let number = |n: Option<u32>| {
        div()
            .flex_shrink_0()
            .w(rems(2.75))
            .pr_2()
            .flex()
            .justify_end()
            .text_color(theme.muted_foreground.opacity(0.7))
            .children(n.map(|n| n.to_string()))
    };
    let (old, new, sign, text, bg, fg): (Option<u32>, Option<u32>, &str, &str, Option<Hsla>, Hsla) = match line {
        DiffLine::Hunk(text) => (None, None, "", text, Some(theme.tints.blue.base.opacity(0.08)), theme.muted_foreground),
        DiffLine::Context { old, new, text } => (Some(*old), Some(*new), " ", text, None, theme.foreground),
        DiffLine::Added { new, text } => (None, Some(*new), "+", text, Some(theme.tints.green.base.opacity(0.14)), theme.foreground),
        DiffLine::Removed { old, text } => (Some(*old), None, "-", text, Some(theme.tints.destructive.base.opacity(0.12)), theme.foreground),
        DiffLine::Note(text) => (None, None, "", text, None, theme.muted_foreground),
    };
    let sign_color = match line {
        DiffLine::Added { .. } => theme.tints.green.soft_foreground,
        DiffLine::Removed { .. } => theme.tints.destructive.soft_foreground,
        _ => theme.muted_foreground,
    };
    div()
        .id("diff-line")
        .flex()
        .items_center()
        .h(rems(1.25))
        .w_full()
        .font_family(FONT_MONO)
        .text_size(rems(0.75))
        .whitespace_nowrap()
        .overflow_hidden()
        .when_some(bg, |el, bg| el.bg(bg))
        .child(number(old))
        .child(number(new))
        .child(div().flex_shrink_0().w(rems(1.)).text_color(sign_color).child(sign.to_string()))
        .child(div().flex_1().min_w_0().text_color(fg).child(SharedString::from(text.replace('\t', "    "))))
}

/// Lane colors, cycled through.
fn lane_palette(theme: &Theme) -> Vec<Hsla> {
    let t = &theme.tints;
    vec![t.blue.base, t.violet.base, t.green.base, t.amber.base, t.rose.base, t.cyan.base]
}

/// Draws a commit's piece of the branch graph: its lines (straight down,
/// or curving between lanes) and its dot (a ring for merges).
fn paint_graph(bounds: Bounds<Pixels>, row: &GraphRow, palette: &[Hsla], background: Hsla, window: &mut Window) {
    let height = bounds.size.height;
    let lane = height * (LANE / HISTORY_ROW);
    let x = |lane_ix: usize| bounds.left() + lane * (lane_ix as f32 + 0.5) + lane * 0.25;
    let y = |at: f32| bounds.top() + height * at;
    let color = |ix: usize| palette[ix % palette.len()];
    let stroke = (lane * 0.14).max(px(1.));
    for segment in &row.segments {
        let (from, to) = (point(x(segment.x0), y(segment.y0)), point(x(segment.x1), y(segment.y1)));
        let mut path = PathBuilder::stroke(stroke);
        path.move_to(from);
        if segment.x0 == segment.x1 {
            path.line_to(to);
        } else {
            // Leave and arrive vertically.
            let middle = (from.y + to.y) / 2.;
            path.cubic_bezier_to(to, point(from.x, middle), point(to.x, middle));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color(segment.color));
        }
    }
    let radius = lane * 0.32;
    let center = point(x(row.lane), y(0.5));
    let dot = |r: Pixels| Bounds::new(point(center.x - r, center.y - r), size(r * 2., r * 2.));
    window.paint_quad(fill(dot(radius + stroke), background).corner_radii(radius + stroke));
    window.paint_quad(fill(dot(radius), color(row.color)).corner_radii(radius));
    let merge = row.segments.iter().filter(|s| s.y0 == 0.5).count() > 1;
    if merge {
        let inner = radius * 0.45;
        window.paint_quad(fill(dot(inner), background).corner_radii(inner));
    }
}
