//! SFTP windows: files on another machine next to the project machine's,
//! like FileZilla. The left side is the project's machine (this Mac, or a
//! remote project's server), the right side the SFTP server; drag files
//! from one to the other (or double-click a file) to copy it across, and
//! the transfers show at the bottom.
//!
//! The servers are the app's (see [`sites`]): add as many as you like,
//! any SFTP window in any project can use them. Connections and transfers
//! run in the XHarness server (see `services/sftp.rs`), so a remote
//! project's files go straight between its machine and the SFTP server.

mod sites;

pub use sites::Sites;

use std::{
    cmp::Ordering,
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, EventEmitter, ExternalPaths, FocusHandle,
    Focusable, FontWeight, KeyBinding, MouseButton, MouseDownEvent, Pixels, Point, SharedString,
    Subscription, Task, UniformListScrollHandle, Window, div, prelude::*, px, relative, rems, uniform_list,
};

use sites::SavedSite;

use crate::i18n::{t, tf, tn};
use crate::services::files::{self, ListOptions};
use crate::services::projects::SftpSettings;
use crate::services::rpc;
use crate::services::server::HostAddress;
use crate::services::sftp::{self, Auth, Direction, Failure, RemoteEntry, Site, Transfer, TransferState};
use crate::ui::actions::{Cancel, Confirm};
use crate::ui::assets::IconName;
use crate::ui::context_menu::{MenuAt, MenuEntry};
use crate::ui::text_input::TextInput;
use crate::ui::theme::{Theme, radius};
use crate::ui::{self, Segment};
use crate::windows::explorer::{DraggedFiles, format_size, resolve_path};

const KEY_CONTEXT: &str = "Sftp";
/// How often running transfers are checked on.
const POLL_EVERY: Duration = Duration::from_millis(300);
/// Times a lost connection is reconnected in a row before giving up.
const RECONNECTS: u8 = 2;

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("enter", Confirm, context),
        KeyBinding::new("escape", Cancel, context),
    ]);
}

pub enum SftpEvent {
    /// The server or a folder changed: save it.
    Changed(SftpSettings),
    Menu {
        entries: Vec<MenuEntry>,
        position: MenuAt,
    },
    Confirm {
        title: String,
        description: String,
        confirm: String,
        action: Rc<dyn Fn(&mut App)>,
    },
}

/// Files being dragged out of an SFTP window's server side.
#[derive(Clone)]
pub struct DraggedRemote {
    session: String,
    paths: Vec<String>,
}

/// What follows the pointer while dragging.
struct DragLabel(SharedString);

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Local,
    Remote,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
    Size,
    Modified,
}

/// A file or folder in either list.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    name: String,
    /// Local paths as the project machine writes them; remote ones with `/`s.
    path: String,
    is_dir: bool,
    is_link: bool,
    size: u64,
    modified: Option<u64>,
    permissions: Option<u32>,
    /// The `..` row.
    up: bool,
}

impl Row {
    fn local(entry: files::Entry) -> Self {
        Row {
            modified: entry
                .modified
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_secs()),
            name: entry.name,
            path: entry.path.to_string_lossy().into_owned(),
            is_dir: entry.is_dir,
            is_link: false,
            size: entry.size,
            permissions: None,
            up: false,
        }
    }

    fn remote(entry: RemoteEntry) -> Self {
        Row {
            name: entry.name,
            path: entry.path,
            is_dir: entry.is_dir,
            is_link: entry.is_link,
            size: entry.size,
            modified: entry.modified,
            permissions: entry.permissions,
            up: false,
        }
    }

    fn up(parent: String) -> Self {
        Row {
            name: "..".into(),
            path: parent,
            is_dir: true,
            is_link: false,
            size: 0,
            modified: None,
            permissions: None,
            up: true,
        }
    }
}

/// One side: a folder and what's in it.
struct Pane {
    side: Side,
    dir: String,
    rows: Vec<Row>,
    selected: HashSet<String>,
    /// Where a shift-click selects from.
    anchor: Option<usize>,
    loading: bool,
    error: Option<String>,
    sort: SortBy,
    descending: bool,
    scroll: UniformListScrollHandle,
    path_input: Entity<TextInput>,
    editing_path: bool,
    _load: Option<Task<()>>,
}

impl Pane {
    fn new(side: Side, cx: &mut Context<SftpView>) -> Self {
        let placeholder = match side {
            Side::Local => t("A folder: ~/dev, ../lib, /tmp"),
            Side::Remote => t("A folder on the server: /var/www, ~/logs"),
        };
        Pane {
            side,
            dir: String::new(),
            rows: Vec::new(),
            selected: HashSet::new(),
            anchor: None,
            loading: false,
            error: None,
            sort: SortBy::Name,
            descending: false,
            scroll: UniformListScrollHandle::new(),
            path_input: cx.new(|cx| TextInput::new(placeholder, cx).inline()),
            editing_path: false,
            _load: None,
        }
    }

    fn parent(&self) -> Option<String> {
        match self.side {
            Side::Local => Path::new(&self.dir).parent().map(|p| p.to_string_lossy().into_owned()),
            Side::Remote => (self.dir != "/" && !self.dir.is_empty()).then(|| sftp::parent(&self.dir)),
        }
    }

    /// Shows `rows` (sorted, under a `..`), keeping what's still there
    /// selected.
    fn set_rows(&mut self, mut rows: Vec<Row>) {
        rows.sort_by(|a, b| compare(a, b, self.sort, self.descending));
        if let Some(parent) = self.parent() {
            rows.insert(0, Row::up(parent));
        }
        self.selected.retain(|path| rows.iter().any(|row| !row.up && &row.path == path));
        self.rows = rows;
        self.anchor = None;
    }

    fn resort(&mut self) {
        let rows: Vec<Row> = self.rows.iter().filter(|row| !row.up).cloned().collect();
        self.set_rows(rows);
    }

    /// The selected rows, in list order.
    fn selection(&self) -> Vec<Row> {
        self.rows.iter().filter(|row| !row.up && self.selected.contains(&row.path)).cloned().collect()
    }

    /// What a drag from row `ix` carries: the selection if the row is in
    /// it, else the row.
    fn dragged(&self, ix: usize) -> Vec<Row> {
        let Some(row) = self.rows.get(ix) else {
            return Vec::new();
        };
        if self.selected.contains(&row.path) { self.selection() } else { vec![row.clone()] }
    }

    fn click(&mut self, ix: usize, extend: bool, toggle: bool) {
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        if row.up {
            self.selected.clear();
            self.anchor = None;
            return;
        }
        if extend && let Some(anchor) = self.anchor {
            let (from, to) = if anchor <= ix { (anchor, ix) } else { (ix, anchor) };
            self.selected = self.rows[from..=to].iter().filter(|r| !r.up).map(|r| r.path.clone()).collect();
            return;
        }
        if toggle {
            if !self.selected.remove(&row.path) {
                self.selected.insert(row.path.clone());
            }
        } else {
            self.selected = HashSet::from([row.path.clone()]);
        }
        self.anchor = Some(ix);
    }
}

/// Folders first, then by the column (names in natural order).
fn compare(a: &Row, b: &Row, sort: SortBy, descending: bool) -> Ordering {
    let by_column = match sort {
        SortBy::Name => natural(&a.name, &b.name),
        SortBy::Size => a.size.cmp(&b.size).then_with(|| natural(&a.name, &b.name)),
        SortBy::Modified => a.modified.cmp(&b.modified).then_with(|| natural(&a.name, &b.name)),
    };
    b.is_dir.cmp(&a.is_dir).then(if descending { by_column.reverse() } else { by_column })
}

/// "file2" before "file10", ignoring case.
fn natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let number = |chars: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(c) = chars.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        chars.next();
                    }
                    digits
                };
                let (x, y) = (number(&mut a), number(&mut b));
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                match x.len().cmp(&y.len()).then_with(|| x.cmp(y)) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(x), Some(y)) => {
                match x.to_lowercase().cmp(y.to_lowercase()) {
                    Ordering::Equal => {}
                    other => return other,
                }
                a.next();
                b.next();
            }
        }
    }
}

/// The connection to the window's server.
#[derive(Clone, Debug, PartialEq)]
enum Link {
    /// No server picked.
    Idle,
    Connecting { site: String },
    /// Asking whether to trust the server's key (first connection).
    Trust { site: String, fingerprint: String },
    Connected { site: String, session: String, home: String },
    Failed { site: String, error: String },
}

impl Link {
    fn site(&self) -> Option<&str> {
        match self {
            Link::Idle => None,
            Link::Connecting { site }
            | Link::Trust { site, .. }
            | Link::Connected { site, .. }
            | Link::Failed { site, .. } => Some(site),
        }
    }
}

/// A name being typed on the server side: a new folder, or a new name.
struct NamePrompt {
    input: Entity<TextInput>,
    /// The path being renamed; `None` makes a folder.
    renaming: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    Agent,
    Password,
    KeyFile,
}

/// The server list and the form for one of them (FileZilla's Site Manager).
struct SiteEditor {
    /// The server being edited; `None` for a new one.
    id: Option<String>,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    auth: AuthKind,
    password: Entity<TextInput>,
    key_path: Entity<TextInput>,
    passphrase: Entity<TextInput>,
    remote_dir: Entity<TextInput>,
    error: Option<String>,
}

impl SiteEditor {
    fn new(site: Option<&SavedSite>, window: &mut Window, cx: &mut Context<SftpView>) -> Self {
        let blank = SavedSite::default();
        let site = site.unwrap_or(&blank);
        let input = |placeholder: &'static str, text: String, cx: &mut Context<SftpView>| {
            cx.new(|cx| TextInput::new(t(placeholder), cx).with_text(text))
        };
        let (auth, password, key_path, passphrase) = match &site.site.auth {
            Auth::Agent => (AuthKind::Agent, String::new(), String::new(), String::new()),
            Auth::Password { password } => (AuthKind::Password, password.clone(), String::new(), String::new()),
            Auth::KeyFile { path, passphrase } => {
                (AuthKind::KeyFile, String::new(), path.to_string_lossy().into_owned(), passphrase.clone())
            }
        };
        let port = if site.site.port == 0 { String::new() } else { site.site.port.to_string() };
        let editor = SiteEditor {
            id: (!site.id.is_empty()).then(|| site.id.clone()),
            name: input("Production", site.name.clone(), cx),
            host: input("example.com", site.site.host.clone(), cx),
            port: input("22", port, cx),
            user: input("deploy", site.site.user.clone(), cx),
            auth,
            password: cx.new(|cx| TextInput::new(t("Password"), cx).masked().with_text(password)),
            key_path: input("~/.ssh/id_ed25519", key_path, cx),
            passphrase: cx.new(|cx| TextInput::new(t("None"), cx).masked().with_text(passphrase)),
            remote_dir: input("/var/www", site.remote_dir.clone(), cx),
            error: None,
        };
        let first = if editor.id.is_some() { &editor.host } else { &editor.name };
        window.focus(&first.focus_handle(cx));
        editor
    }

    /// The server as typed, or what's missing.
    fn read(&self, cx: &App) -> Result<SavedSite, String> {
        let text = |input: &Entity<TextInput>| input.read(cx).text().trim().to_string();
        let host = text(&self.host);
        let user = text(&self.user);
        if host.is_empty() {
            return Err(t("Type the server's address.").into());
        }
        if user.is_empty() {
            return Err(t("Type the user name to sign in with.").into());
        }
        let port = match text(&self.port) {
            port if port.is_empty() => 22,
            port => port.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| t("The port is a number from 1 to 65535.").to_string())?,
        };
        let auth = match self.auth {
            AuthKind::Agent => Auth::Agent,
            // Passwords keep their spaces.
            AuthKind::Password => Auth::Password { password: self.password.read(cx).text().to_string() },
            AuthKind::KeyFile => {
                let path = text(&self.key_path);
                if path.is_empty() {
                    return Err(t("Pick the key file to sign in with.").into());
                }
                Auth::KeyFile { path: PathBuf::from(path), passphrase: self.passphrase.read(cx).text().to_string() }
            }
        };
        Ok(SavedSite {
            id: self.id.clone().unwrap_or_else(sites::new_id),
            name: text(&self.name),
            site: Site { host, port, user, auth },
            remote_dir: text(&self.remote_dir),
        })
    }
}

pub struct SftpView {
    /// The project's machine: where the left side's files are, and where
    /// connections are made from (`None`: this Mac).
    server: Option<HostAddress>,
    settings: SftpSettings,
    link: Link,
    local: Pane,
    remote: Pane,
    transfers: Vec<Transfer>,
    prompt: Option<NamePrompt>,
    editor: Option<SiteEditor>,
    focus_handle: FocusHandle,
    /// Where its dropdown buttons are (see [`ui::Anchors`]).
    anchors: ui::Anchors,
    /// Reconnects since the last call that worked.
    reconnects: u8,
    _connect: Option<Task<()>>,
    _poll: Option<Task<()>>,
    _sites: Subscription,
}

impl EventEmitter<SftpEvent> for SftpView {}

impl Focusable for SftpView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Drop for SftpView {
    /// A closed window ends its connection, unless something is still
    /// being copied (the server finishes it, then closes it when idle).
    fn drop(&mut self) {
        if let Link::Connected { session, .. } = &self.link
            && self.transfers.iter().all(|transfer| transfer.state.finished())
        {
            let (server, session) = (self.server.clone(), session.clone());
            std::thread::spawn(move || rpc::on(server.as_ref(), || sftp::disconnect(&session)));
        }
    }
}

impl SftpView {
    /// `folder` is where the left side starts when the window has none
    /// saved (the project's folder).
    pub fn new(
        settings: SftpSettings,
        folder: Option<PathBuf>,
        server: Option<HostAddress>,
        cx: &mut Context<Self>,
    ) -> Self {
        Sites::ensure(cx);
        // A server edited or removed (here or in another window).
        let sites = cx.observe_global::<Sites>(|this, cx| {
            if let Some(site) = this.link.site()
                && Sites::get(site, cx).is_none()
            {
                this.disconnect(cx);
                this.settings.site = None;
                this.emit_changed(cx);
            }
            cx.notify();
        });
        let mut this = SftpView {
            server,
            settings: settings.clone(),
            link: Link::Idle,
            local: Pane::new(Side::Local, cx),
            remote: Pane::new(Side::Remote, cx),
            transfers: Vec::new(),
            prompt: None,
            editor: None,
            focus_handle: cx.focus_handle(),
            anchors: ui::Anchors::default(),
            reconnects: 0,
            _connect: None,
            _poll: None,
            _sites: sites,
        };
        let local = settings.local.or(folder);
        match local {
            Some(dir) => {
                this.local.dir = dir.to_string_lossy().into_owned();
                this.load(Side::Local, cx);
            }
            None => this.go_home(cx),
        }
        // Back to the server it was on.
        if let Some(site) = settings.site.filter(|site| Sites::get(site, cx).is_some()) {
            this.connect(site, None, cx);
        }
        this
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// Something is being copied.
    pub fn busy(&self) -> bool {
        self.transfers.iter().any(|transfer| !transfer.state.finished())
    }

    /// For the window's title: where it's connected.
    pub fn summary(&self, cx: &App) -> Option<String> {
        match &self.link {
            Link::Connected { site, .. } => {
                let site = Sites::get(site, cx)?;
                Some(format!("{}:{}", site.site.address(), self.remote.dir))
            }
            Link::Connecting { site } => Sites::get(site, cx).map(|site| tf("Connecting to {}…", &[&site.label()])),
            _ => None,
        }
    }

    fn session(&self) -> Option<&str> {
        match &self.link {
            Link::Connected { session, .. } => Some(session),
            _ => None,
        }
    }

    fn emit_changed(&mut self, cx: &mut Context<Self>) {
        self.settings.local = (!self.local.dir.is_empty()).then(|| PathBuf::from(&self.local.dir));
        if matches!(self.link, Link::Connected { .. }) {
            self.settings.remote = (!self.remote.dir.is_empty()).then(|| self.remote.dir.clone());
        }
        cx.emit(SftpEvent::Changed(self.settings.clone()));
    }

    /// Runs `work` in the background, against the project's machine, then
    /// `done` with what it returned.
    fn call<R: Send + 'static>(
        &self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) -> Task<()> {
        let server = self.server.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { rpc::on(server.as_ref(), work) }).await;
            this.update(cx, |this, cx| done(this, result, cx)).ok();
        })
    }

    // Connecting ---------------------------------------------------------------------

    /// Connects to the server `site` (from the list), dropping any other
    /// connection. `trust` is the fingerprint you agreed to.
    fn connect(&mut self, site: String, trust: Option<String>, cx: &mut Context<Self>) {
        let Some(saved) = Sites::get(&site, cx) else {
            return;
        };
        if self.link.site() != Some(site.as_str()) {
            // Another server: its folders start fresh.
            self.settings.remote = None;
        }
        self.disconnect(cx);
        self.settings.site = Some(site.clone());
        self.link = Link::Connecting { site: site.clone() };
        self.remote.rows.clear();
        self.remote.error = None;
        self.emit_changed(cx);
        let target = site.clone();
        let to = saved.site.clone();
        self._connect = Some(self.call(
            cx,
            move || sftp::connect(&to, trust.as_deref()),
            move |this, connected, cx| {
                // Another server was picked meanwhile.
                if this.link != (Link::Connecting { site: target.clone() }) {
                    if let sftp::Connected::Ok { session, .. } = connected {
                        let server = this.server.clone();
                        std::thread::spawn(move || rpc::on(server.as_ref(), || sftp::disconnect(&session)));
                    }
                    return;
                }
                this.link = match connected {
                    sftp::Connected::Ok { session, home } => {
                        let start = this
                            .settings
                            .remote
                            .clone()
                            .or_else(|| (!saved.remote_dir.is_empty()).then(|| expand_remote(&saved.remote_dir, &home)))
                            .unwrap_or_else(|| home.clone());
                        this.remote.dir = start;
                        Link::Connected { site: target, session, home }
                    }
                    sftp::Connected::UnknownHost { fingerprint } => Link::Trust { site: target, fingerprint },
                    sftp::Connected::ChangedHost { fingerprint } => Link::Failed {
                        site: target,
                        error: tf(
                            "{}'s key has changed (it's now {}). Someone may be listening in, or the server was set up again. If you expected it, remove its old key from ~/.ssh/known_hosts and connect again.",
                            &[&saved.site.host, &fingerprint],
                        ),
                    },
                    sftp::Connected::Failed(error) => Link::Failed { site: target, error },
                };
                if matches!(this.link, Link::Connected { .. }) {
                    this.load(Side::Remote, cx);
                    this.poll(cx);
                }
                cx.notify();
            },
        ));
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        if let Link::Connected { session, .. } = &self.link {
            let session = session.clone();
            self.call(cx, move || sftp::disconnect(&session), |_, _, _| {}).detach();
        }
        self._connect = None;
        self._poll = None;
        self.transfers.clear();
        self.link = Link::Idle;
        self.prompt = None;
        self.remote.rows.clear();
        self.remote.selected.clear();
        cx.notify();
    }

    /// The connection went away: connect again (a couple of times), back
    /// to the same folder.
    fn lost(&mut self, cx: &mut Context<Self>) {
        let Some(site) = self.link.site().map(str::to_string) else {
            return;
        };
        let folder = self.remote.dir.clone();
        if self.reconnects >= RECONNECTS {
            self.disconnect(cx);
            self.link = Link::Failed { site, error: t("The connection was closed.").into() };
            cx.notify();
            return;
        }
        self.reconnects += 1;
        self.connect(site, None, cx);
        self.settings.remote = Some(folder);
    }

    // Listing and moving around ----------------------------------------------------------

    fn pane(&self, side: Side) -> &Pane {
        match side {
            Side::Local => &self.local,
            Side::Remote => &self.remote,
        }
    }

    fn pane_mut(&mut self, side: Side) -> &mut Pane {
        match side {
            Side::Local => &mut self.local,
            Side::Remote => &mut self.remote,
        }
    }

    fn go_home(&mut self, cx: &mut Context<Self>) {
        self.local._load = Some(self.call(cx, files::home, |this, home, cx| {
            this.local.dir = home.unwrap_or_else(|| PathBuf::from("/")).to_string_lossy().into_owned();
            this.load(Side::Local, cx);
        }));
    }

    /// Lists the side's folder again.
    fn load(&mut self, side: Side, cx: &mut Context<Self>) {
        let dir = self.pane(side).dir.clone();
        if dir.is_empty() {
            return;
        }
        let task = match side {
            Side::Local => {
                let options = ListOptions { show_hidden: true, show_ignored: true };
                self.call(
                    cx,
                    move || files::list_dir(Path::new(&dir), options),
                    |this, entries, cx| {
                        this.local.loading = false;
                        this.local.set_rows(entries.into_iter().map(Row::local).collect());
                        cx.notify();
                    },
                )
            }
            Side::Remote => {
                let Some(session) = self.session().map(str::to_string) else {
                    return;
                };
                self.call(
                    cx,
                    move || sftp::list(&session, &dir),
                    |this, listed, cx| {
                        this.remote.loading = false;
                        match listed {
                            Ok(entries) => {
                                this.reconnects = 0;
                                this.remote.error = None;
                                this.remote.set_rows(entries.into_iter().map(Row::remote).collect());
                            }
                            Err(Failure::Disconnected) => this.lost(cx),
                            Err(Failure::Failed(error)) => {
                                this.remote.error = Some(error);
                                this.remote.rows.clear();
                                if let Some(parent) = this.remote.parent() {
                                    this.remote.rows.push(Row::up(parent));
                                }
                            }
                        }
                        cx.notify();
                    },
                )
            }
        };
        let pane = self.pane_mut(side);
        pane.loading = true;
        pane._load = Some(task);
        cx.notify();
    }

    fn open_dir(&mut self, side: Side, dir: String, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        pane.dir = dir;
        pane.selected.clear();
        pane.error = None;
        pane.editing_path = false;
        pane.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        self.load(side, cx);
        self.emit_changed(cx);
    }

    fn go_up(&mut self, side: Side, cx: &mut Context<Self>) {
        if let Some(parent) = self.pane(side).parent() {
            self.open_dir(side, parent, cx);
        }
    }

    fn start_editing_path(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        pane.editing_path = true;
        let text = match side {
            Side::Local => ui::display_path(Path::new(&pane.dir)),
            Side::Remote => pane.dir.clone(),
        };
        pane.path_input.update(cx, |input, cx| {
            input.set_text(text, cx);
            input.select_all_text(cx);
        });
        window.focus(&pane.path_input.focus_handle(cx));
        cx.notify();
    }

    /// Goes to the folder typed in the side's path bar.
    fn go_to_typed(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.pane(side).path_input.read(cx).text().trim().to_string();
        window.focus(&self.focus_handle);
        if typed.is_empty() {
            self.pane_mut(side).editing_path = false;
            cx.notify();
            return;
        }
        match side {
            Side::Local => {
                let dir = resolve_path(&typed, Path::new(&self.local.dir));
                self.open_dir(Side::Local, dir.to_string_lossy().into_owned(), cx);
            }
            Side::Remote => {
                let Link::Connected { session, home, .. } = &self.link else {
                    return;
                };
                let path = match expand_remote(&typed, home) {
                    path if path.starts_with('/') => path,
                    relative => sftp::join(&self.remote.dir, &relative),
                };
                let session = session.clone();
                self.remote.editing_path = false;
                self.call(
                    cx,
                    move || sftp::canonicalize(&session, &path),
                    move |this, found, cx| match found {
                        Ok(dir) => this.open_dir(Side::Remote, dir, cx),
                        Err(Failure::Disconnected) => this.lost(cx),
                        Err(Failure::Failed(_)) => {
                            this.remote.error = Some(tf("{} isn't there.", &[&typed]));
                            cx.notify();
                        }
                    },
                )
                .detach();
            }
        }
    }

    fn sort_by(&mut self, side: Side, sort: SortBy, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        if pane.sort == sort {
            pane.descending = !pane.descending;
        } else {
            pane.sort = sort;
            pane.descending = false;
        }
        pane.resort();
        cx.notify();
    }

    /// Double-click: into a folder, or a file across to the other side.
    fn activate(&mut self, side: Side, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.pane(side).rows.get(ix).cloned() else {
            return;
        };
        if row.is_dir {
            self.open_dir(side, row.path, cx);
        } else {
            self.send(side, vec![row], None, cx);
        }
    }

    // Copying ------------------------------------------------------------------------

    /// Copies `rows` from `side` to the other side's folder (or `into`).
    fn send(&mut self, side: Side, rows: Vec<Row>, into: Option<String>, cx: &mut Context<Self>) {
        let rows: Vec<Row> = rows.into_iter().filter(|row| !row.up).collect();
        if rows.is_empty() {
            return;
        }
        match side {
            Side::Local => self.upload(rows.into_iter().map(|row| PathBuf::from(row.path)).collect(), into, cx),
            Side::Remote => self.download(rows.into_iter().map(|row| row.path).collect(), into.map(PathBuf::from), cx),
        }
    }

    fn upload(&mut self, paths: Vec<PathBuf>, into: Option<String>, cx: &mut Context<Self>) {
        let Some(session) = self.session().map(str::to_string) else {
            self.remote.error = Some(t("Connect to a server first.").into());
            cx.notify();
            return;
        };
        let into = into.unwrap_or_else(|| self.remote.dir.clone());
        self.call(cx, move || sftp::upload(&session, &paths, &into), Self::started).detach();
    }

    fn download(&mut self, paths: Vec<String>, into: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(session) = self.session().map(str::to_string) else {
            return;
        };
        let into = into.unwrap_or_else(|| PathBuf::from(&self.local.dir));
        self.call(cx, move || sftp::download(&session, &paths, &into), Self::started).detach();
    }

    fn started(&mut self, started: Result<u64, Failure>, cx: &mut Context<Self>) {
        match started {
            Ok(_) => self.poll(cx),
            Err(Failure::Disconnected) => self.lost(cx),
            Err(Failure::Failed(error)) => {
                self.remote.error = Some(error);
                cx.notify();
            }
        }
    }

    /// Checks on the transfers until none is running; each that finishes
    /// lists its folder again.
    fn poll(&mut self, cx: &mut Context<Self>) {
        if self._poll.is_some() {
            return;
        }
        let server = self.server.clone();
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(Some(session)) = this.read_with(cx, |this, _| this.session().map(str::to_string)) else {
                    break;
                };
                let server = server.clone();
                let transfers = cx
                    .background_executor()
                    .spawn(async move { rpc::on(server.as_ref(), || sftp::transfers(&session)) })
                    .await;
                let running = this.update(cx, |this, cx| this.apply_transfers(transfers, cx)).unwrap_or(false);
                if !running {
                    break;
                }
                cx.background_executor().timer(POLL_EVERY).await;
            }
            this.update(cx, |this, _| this._poll = None).ok();
        }));
    }

    fn apply_transfers(&mut self, transfers: Vec<Transfer>, cx: &mut Context<Self>) -> bool {
        for transfer in &transfers {
            let was_running = self
                .transfers
                .iter()
                .find(|old| old.id == transfer.id)
                .is_none_or(|old| !old.state.finished());
            if was_running && transfer.state.finished() {
                match transfer.direction {
                    Direction::Upload => self.load(Side::Remote, cx),
                    Direction::Download => self.load(Side::Local, cx),
                }
            }
        }
        self.transfers = transfers;
        cx.notify();
        self.busy()
    }

    fn cancel_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        self.call(cx, move || sftp::cancel(id), |this, _, cx| this.poll(cx)).detach();
    }

    fn clear_finished(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session().map(str::to_string) else {
            return;
        };
        self.transfers.retain(|transfer| !transfer.state.finished());
        self.call(cx, move || sftp::clear_finished(&session), |_, _, _| {}).detach();
        cx.notify();
    }

    /// Drops on the server side: files from the left side or an Explorer
    /// window go up; the server's own files move into `into` (a folder row).
    fn drop_on_remote(&mut self, files: Vec<PathBuf>, into: Option<String>, cx: &mut Context<Self>) {
        self.upload(files, into, cx);
    }

    fn drop_remote_on_remote(&mut self, dragged: &DraggedRemote, into: String, cx: &mut Context<Self>) {
        let Some(session) = self.session().map(str::to_string) else {
            return;
        };
        let moves: Vec<(String, String)> = dragged
            .paths
            .iter()
            .filter(|path| **path != into && sftp::parent(path) != into && !into.starts_with(&format!("{path}/")))
            .map(|path| (path.clone(), sftp::join(&into, sftp::file_name(path))))
            .collect();
        if dragged.session != session || moves.is_empty() {
            return;
        }
        self.call(
            cx,
            move || moves.iter().try_for_each(|(from, to)| sftp::rename(&session, from, to)),
            Self::after_change,
        )
        .detach();
    }

    fn drop_on_local(&mut self, dragged: &DraggedRemote, into: Option<String>, cx: &mut Context<Self>) {
        if self.session() == Some(dragged.session.as_str()) {
            self.download(dragged.paths.clone(), into.map(PathBuf::from), cx);
        }
    }

    /// Files from Finder: they're on this Mac, so only a project here can
    /// send them.
    fn drop_external(&mut self, paths: Vec<PathBuf>, into: Option<String>, cx: &mut Context<Self>) {
        if self.server.is_some() {
            self.remote.error = Some(t("Files from this Mac can't be sent from a remote project: drag them from its Explorer instead.").into());
            cx.notify();
            return;
        }
        self.upload(paths, into, cx);
    }

    // Changing the server's files ----------------------------------------------------------

    fn after_change(&mut self, result: Result<(), Failure>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => self.load(Side::Remote, cx),
            Err(Failure::Disconnected) => self.lost(cx),
            Err(Failure::Failed(error)) => {
                self.remote.error = Some(error);
                self.load(Side::Remote, cx);
            }
        }
    }

    fn start_prompt(&mut self, renaming: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let text = renaming.as_deref().map(sftp::file_name).unwrap_or_default().to_string();
        let input = cx.new(|cx| TextInput::new(t("Folder name"), cx).with_text(text));
        input.update(cx, |input, cx| input.select_all_text(cx));
        window.focus(&input.focus_handle(cx));
        self.prompt = Some(NamePrompt { input, renaming });
        cx.notify();
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        window.focus(&self.focus_handle);
        let name = prompt.input.read(cx).text().trim().to_string();
        let Some(session) = self.session().map(str::to_string) else {
            return;
        };
        if name.is_empty() || name.contains('/') {
            cx.notify();
            return;
        }
        let target = sftp::join(&self.remote.dir, &name);
        let work: Box<dyn FnOnce() -> Result<(), Failure> + Send> = match prompt.renaming {
            Some(from) if from == target => return cx.notify(),
            Some(from) => Box::new(move || sftp::rename(&session, &from, &target)),
            None => Box::new(move || sftp::create_dir(&session, &target)),
        };
        self.call(cx, work, Self::after_change).detach();
        cx.notify();
    }

    fn confirm_delete(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) {
        let rows: Vec<Row> = rows.into_iter().filter(|row| !row.up).collect();
        let Some(first) = rows.first() else {
            return;
        };
        let title = match rows.as_slice() {
            [one] => tf("Delete “{}” from the server?", &[&one.name]),
            many => tn(many.len(), "Delete {} item from the server?", "Delete {} items from the server?"),
        };
        let description = if rows.iter().any(|row| row.is_dir) {
            t("Folders go with everything in them. This can't be undone.").to_string()
        } else {
            t("This can't be undone.").to_string()
        };
        let _ = first;
        let paths: Vec<String> = rows.into_iter().map(|row| row.path).collect();
        let view = cx.entity().downgrade();
        cx.emit(SftpEvent::Confirm {
            title,
            description,
            confirm: t("Delete").into(),
            action: Rc::new(move |cx: &mut App| {
                let paths = paths.clone();
                view.update(cx, |this, cx| {
                    let Some(session) = this.session().map(str::to_string) else {
                        return;
                    };
                    this.call(cx, move || sftp::remove(&session, &paths), Self::after_change).detach();
                })
                .ok();
            }),
        });
    }

    // Menus ------------------------------------------------------------------------

    fn row_menu(&mut self, side: Side, ix: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        let Some(row) = pane.rows.get(ix).cloned() else {
            return;
        };
        if row.up {
            return;
        }
        if !pane.selected.contains(&row.path) {
            pane.selected = HashSet::from([row.path.clone()]);
            pane.anchor = Some(ix);
        }
        let rows = pane.selection();
        let view = cx.entity().downgrade();
        let act = |f: Rc<dyn Fn(&mut SftpView, &mut Window, &mut Context<SftpView>)>| {
            let view = view.clone();
            move |window: &mut Window, cx: &mut App| {
                view.update(cx, |this, cx| f(this, window, cx)).ok();
            }
        };
        let connected = self.session().is_some();
        let mut entries = Vec::new();
        match side {
            Side::Local => {
                if connected {
                    let label = match rows.as_slice() {
                        [one] => tf("Upload “{}”", &[&one.name]),
                        many => tn(many.len(), "Upload {} item", "Upload {} items"),
                    };
                    let sent = rows.clone();
                    entries.push(MenuEntry::item(IconName::Upload, label, act(Rc::new(move |this, _, cx| this.send(Side::Local, sent.clone(), None, cx)))));
                }
                if row.is_dir && rows.len() == 1 {
                    let dir = row.path.clone();
                    entries.push(MenuEntry::item(IconName::FolderOpen, t("Open"), act(Rc::new(move |this, _, cx| this.open_dir(Side::Local, dir.clone(), cx)))));
                }
                entries.push(MenuEntry::Separator);
                entries.push(MenuEntry::item(IconName::RefreshCw, t("Refresh"), act(Rc::new(|this, _, cx| this.load(Side::Local, cx)))));
            }
            Side::Remote => {
                let label = match rows.as_slice() {
                    [one] => tf("Download “{}”", &[&one.name]),
                    many => tn(many.len(), "Download {} item", "Download {} items"),
                };
                let sent = rows.clone();
                entries.push(MenuEntry::item(IconName::Download, label, act(Rc::new(move |this, _, cx| this.send(Side::Remote, sent.clone(), None, cx)))));
                if row.is_dir && rows.len() == 1 {
                    let dir = row.path.clone();
                    entries.push(MenuEntry::item(IconName::FolderOpen, t("Open"), act(Rc::new(move |this, _, cx| this.open_dir(Side::Remote, dir.clone(), cx)))));
                }
                entries.push(MenuEntry::Separator);
                entries.push(MenuEntry::item(IconName::FolderPlus, t("New folder"), act(Rc::new(|this, window, cx| this.start_prompt(None, window, cx)))));
                if rows.len() == 1 {
                    let path = row.path.clone();
                    entries.push(MenuEntry::item(IconName::Pencil, t("Rename"), act(Rc::new(move |this, window, cx| this.start_prompt(Some(path.clone()), window, cx)))));
                    let path = row.path.clone();
                    entries.push(MenuEntry::item(IconName::Copy, t("Copy path"), move |_, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.clone()));
                    }));
                }
                entries.push(MenuEntry::item(IconName::RefreshCw, t("Refresh"), act(Rc::new(|this, _, cx| this.load(Side::Remote, cx)))));
                entries.push(MenuEntry::Separator);
                let doomed = rows.clone();
                entries.push(MenuEntry::destructive(IconName::Trash, t("Delete…"), act(Rc::new(move |this, _, cx| this.confirm_delete(doomed.clone(), cx)))));
            }
        }
        cx.emit(SftpEvent::Menu { entries, position: position.into() });
        cx.notify();
    }

    /// The server picker: every server, then adding and managing them.
    fn sites_menu(&mut self, position: MenuAt, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        let current = self.link.site().map(str::to_string);
        let mut entries: Vec<MenuEntry> = Sites::all(cx)
            .into_iter()
            .map(|site| {
                let view = view.clone();
                let checked = current.as_deref() == Some(site.id.as_str());
                MenuEntry::check(format!("{}  ·  {}", site.label(), site.site.address()), checked, move |_, cx| {
                    let id = site.id.clone();
                    view.update(cx, |this, cx| {
                        this.reconnects = 0;
                        this.connect(id, None, cx);
                    })
                    .ok();
                })
            })
            .collect();
        if !entries.is_empty() {
            entries.push(MenuEntry::Separator);
        }
        let add = view.clone();
        entries.push(MenuEntry::item(IconName::Plus, t("Add a server…"), move |window, cx| {
            add.update(cx, |this, cx| this.open_editor(None, window, cx)).ok();
        }));
        entries.push(MenuEntry::item(IconName::Settings, t("Manage servers…"), move |window, cx| {
            view.update(cx, |this, cx| {
                let site = this.link.site().map(str::to_string).or_else(|| Sites::all(cx).first().map(|s| s.id.clone()));
                this.open_editor(site, window, cx)
            })
            .ok();
        }));
        cx.emit(SftpEvent::Menu { entries, position });
    }

    // The server list ------------------------------------------------------------------

    fn open_editor(&mut self, site: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let saved = site.and_then(|id| Sites::get(&id, cx));
        self.editor = Some(SiteEditor::new(saved.as_ref(), window, cx));
        cx.notify();
    }

    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Saves the server in the form; `connect` connects to it too.
    fn save_editor(&mut self, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let site = match editor.read(cx) {
            Ok(site) => site,
            Err(error) => {
                editor.error = Some(error);
                cx.notify();
                return;
            }
        };
        let id = site.id.clone();
        Sites::put(site, cx);
        self.close_editor(window, cx);
        if connect {
            self.reconnects = 0;
            self.connect(id, None, cx);
        }
    }

    fn confirm_remove_site(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(site) = Sites::get(&id, cx) else {
            return;
        };
        let view = cx.entity().downgrade();
        cx.emit(SftpEvent::Confirm {
            title: tf("Remove “{}”?", &[&site.label()]),
            description: t("It goes from the server list of every project. Files on the server stay.").into(),
            confirm: t("Remove").into(),
            action: Rc::new(move |cx: &mut App| {
                Sites::remove(&id, cx);
                view.update(cx, |this, cx| {
                    if this.editor.as_ref().is_some_and(|e| e.id.as_deref() == Some(id.as_str())) {
                        this.editor = None;
                    }
                    cx.notify();
                })
                .ok();
            }),
        });
    }

    // Keys -------------------------------------------------------------------------

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt.is_some() {
            self.submit_prompt(window, cx);
        } else if self.editor.is_some() {
            self.save_editor(false, window, cx);
        } else if self.local.editing_path {
            self.go_to_typed(Side::Local, window, cx);
        } else if self.remote.editing_path {
            self.go_to_typed(Side::Remote, window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt.take().is_some() || self.editor.take().is_some() {
            window.focus(&self.focus_handle);
        } else {
            self.local.editing_path = false;
            self.remote.editing_path = false;
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }
}

/// `~` and `~/…` on the server, with its home.
fn expand_remote(path: &str, home: &str) -> String {
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        sftp::join(home, rest)
    } else {
        path.to_string()
    }
}

/// "2026-11-02 14:32", in local time.
fn format_time(secs: u64) -> String {
    let time = secs as libc::time_t;
    // SAFETY: localtime_r/localtime_s write into the tm we pass and nothing
    // else.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    #[cfg(windows)]
    let ok = unsafe { libc::localtime_s(&mut tm, &time) == 0 };
    if !ok {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        1900 + tm.tm_year,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// `drwxr-xr-x`, as `ls -l` shows modes.
fn format_permissions(row: &Row) -> String {
    let Some(mode) = row.permissions else {
        return String::new();
    };
    let mut text = String::with_capacity(10);
    text.push(if row.is_link { 'l' } else if row.is_dir { 'd' } else { '-' });
    for shift in [6, 3, 0] {
        for (bit, letter) in [(4, 'r'), (2, 'w'), (1, 'x')] {
            text.push(if (mode >> shift) & bit != 0 { letter } else { '-' });
        }
    }
    text
}

// Drawing --------------------------------------------------------------------------

/// Column widths after Name.
const SIZE_WIDTH: f32 = 5.5;
const MODIFIED_WIDTH: f32 = 7.5;
const PERMISSIONS_WIDTH: f32 = 5.75;

impl SftpView {
    fn render_toolbar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let site = self.link.site().and_then(|id| Sites::get(id, cx));
        let picker_label = match &site {
            Some(site) => site.label(),
            None if Sites::all(cx).is_empty() => t("No servers yet").to_string(),
            None => t("Pick a server").to_string(),
        };
        let (dot, status) = match &self.link {
            Link::Idle => (None, String::new()),
            Link::Connecting { .. } => (Some(theme.tints.amber.base), t("Connecting…").to_string()),
            Link::Trust { .. } => (Some(theme.tints.amber.base), t("Waiting for you to trust the server").to_string()),
            Link::Connected { .. } => (
                Some(theme.tints.green.base),
                site.as_ref().map(|site| tf("Connected to {}", &[&site.site.address()])).unwrap_or_default(),
            ),
            Link::Failed { .. } => (Some(theme.tints.destructive.base), t("Not connected").to_string()),
        };
        let connected = matches!(self.link, Link::Connected { .. } | Link::Connecting { .. } | Link::Trust { .. });
        let (muted, fg, hover) = (theme.muted_foreground, theme.foreground, theme.secondary);
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .h_10()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("sftp-site")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .h_7()
                    .px_2()
                    .max_w(rems(16.))
                    .rounded(rems(0.5))
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(ui::icon(IconName::Server).size_3p5().text_color(muted))
                    .child(div().min_w_0().truncate().text_color(fg).font_weight(FontWeight::MEDIUM).child(picker_label))
                    .child(ui::icon(IconName::ChevronDown).size_3().text_color(muted))
                    .child(self.anchors.track("site"))
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.sites_menu(this.anchors.below("site", ui::Align::Start, event.position()), cx)
                    })),
            )
            .when_some(site.as_ref().map(|site| site.id.clone()), |bar, id| {
                bar.child(if connected {
                    ui::compact_button("sftp-disconnect", Some(IconName::Unplug), t("Disconnect"), false, theme)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.disconnect(cx);
                            this.settings.site = None;
                            this.emit_changed(cx);
                        }))
                } else {
                    ui::compact_button("sftp-connect", Some(IconName::Plug), t("Connect"), true, theme)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reconnects = 0;
                            this.connect(id.clone(), None, cx)
                        }))
                })
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1p5()
                    .text_size(rems(0.75))
                    .text_color(muted)
                    .children(dot.map(ui::status_dot))
                    .child(div().min_w_0().truncate().child(status)),
            )
            .child(
                ui::small_icon_button("sftp-servers", IconName::Settings, theme).on_click(cx.listener(
                    |this, _, window, cx| {
                        let site = this.link.site().map(str::to_string).or_else(|| Sites::all(cx).first().map(|s| s.id.clone()));
                        this.open_editor(site, window, cx)
                    },
                )),
            )
    }

    fn render_path_bar(&self, side: Side, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let pane = self.pane(side);
        let label = match (side, &self.server) {
            (Side::Local, None) => t("This Mac").to_string(),
            (Side::Local, Some(HostAddress::Remote(key))) => key.name.clone(),
            (Side::Local, Some(_)) => t("This Mac").to_string(),
            (Side::Remote, _) => self
                .link
                .site()
                .and_then(|id| Sites::get(id, cx))
                .map(|site| site.label())
                .unwrap_or_else(|| t("Server").to_string()),
        };
        let shown = match side {
            Side::Local => ui::display_path(Path::new(&pane.dir)),
            Side::Remote => pane.dir.clone(),
        };
        let can_act = side == Side::Local || self.session().is_some();
        let (muted, fg, hover) = (theme.muted_foreground, theme.foreground, theme.secondary);
        let id = |name: &str| SharedString::from(format!("{name}-{side:?}"));
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .h_8()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .text_size(rems(0.75))
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(rems(8.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(label),
            )
            .child(if pane.editing_path {
                div()
                    .flex_1()
                    .min_w_0()
                    .h_6()
                    .px_1p5()
                    .flex()
                    .items_center()
                    .rounded(rems(0.375))
                    .border_1()
                    .border_color(theme.border_active)
                    .child(pane.path_input.clone())
                    .into_any_element()
            } else {
                div()
                    .id(id("sftp-path"))
                    .flex_1()
                    .min_w_0()
                    .h_6()
                    .px_1p5()
                    .flex()
                    .items_center()
                    .rounded(rems(0.375))
                    .text_color(muted)
                    .when(can_act, |path| {
                        path.cursor_text()
                            .hover(move |style| style.bg(hover).text_color(fg))
                            .on_click(cx.listener(move |this, _, window, cx| this.start_editing_path(side, window, cx)))
                    })
                    .child(div().min_w_0().truncate().child(shown))
                    .into_any_element()
            })
            .when(can_act, |bar| {
                bar.child(
                    ui::small_icon_button(id("sftp-up"), IconName::ArrowUp, theme)
                        .on_click(cx.listener(move |this, _, _, cx| this.go_up(side, cx))),
                )
                .child(
                    ui::small_icon_button(id("sftp-refresh"), IconName::RefreshCw, theme)
                        .on_click(cx.listener(move |this, _, _, cx| this.load(side, cx))),
                )
                .when(side == Side::Remote, |bar| {
                    bar.child(
                        ui::small_icon_button("sftp-new-folder", IconName::FolderPlus, theme)
                            .on_click(cx.listener(|this, _, window, cx| this.start_prompt(None, window, cx))),
                    )
                })
            })
    }

    fn render_header(&self, side: Side, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let pane = self.pane(side);
        let cell = |label: &'static str, sort: SortBy| {
            let sorted = pane.sort == sort;
            let (muted, fg) = (theme.muted_foreground, theme.foreground);
            div()
                .id(SharedString::from(format!("{label}-{side:?}")))
                .flex()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .text_color(if sorted { fg } else { muted })
                .hover(move |style| style.text_color(fg))
                .child(t(label))
                .when(sorted, |cell| {
                    let arrow = if pane.descending { IconName::ArrowDown } else { IconName::ArrowUp };
                    cell.child(ui::icon(arrow).size_3())
                })
                .on_click(cx.listener(move |this, _, _, cx| this.sort_by(side, sort, cx)))
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
            .font_weight(FontWeight::MEDIUM)
            .child(div().flex_1().min_w_0().pl(rems(1.5)).child(cell("Name", SortBy::Name)))
            .child(div().flex().justify_end().w(rems(SIZE_WIDTH)).child(cell("Size", SortBy::Size)))
            .child(div().w(rems(MODIFIED_WIDTH)).child(cell("Modified", SortBy::Modified)))
            .when(side == Side::Remote, |header| {
                header.child(div().w(rems(PERMISSIONS_WIDTH)).text_color(theme.muted_foreground).child(t("Permissions")))
            })
    }

    fn render_row(&self, side: Side, ix: usize, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let pane = self.pane(side);
        let row = &pane.rows[ix];
        let selected = pane.selected.contains(&row.path) && !row.up;
        let (muted, hover_bg) = (theme.muted_foreground, theme.accent);
        let drop_bg = theme.tints.blue.base.opacity(0.18);
        let icon = if row.up {
            IconName::ArrowUp
        } else if row.is_dir {
            IconName::Folder
        } else {
            IconName::File
        };
        let icon_color = if row.is_dir { theme.tints.blue.base } else { muted };
        // Dropping on a folder puts things in it.
        let into = row.is_dir.then(|| row.path.clone());
        let element_id = ElementId::named_usize(if side == Side::Local { "local-row" } else { "remote-row" }, ix);
        let session = self.session().map(str::to_string);

        let mut item = div()
            .id(element_id)
            .flex()
            .items_center()
            .gap_3()
            .w_full()
            .h_7()
            .px_2()
            .rounded(rems(6. / 16.))
            .map(|item| if selected { item.bg(theme.tints.blue.soft).text_color(theme.foreground) } else { item.hover(move |style| style.bg(hover_bg)) })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle);
                let modifiers = event.modifiers();
                this.pane_mut(side).click(ix, modifiers.shift, modifiers.secondary());
                if event.click_count() == 2 {
                    this.activate(side, ix, cx);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| this.row_menu(side, ix, event.position, cx)),
            );
        if !row.up {
            let dragged = pane.dragged(ix);
            let label: SharedString = match dragged.as_slice() {
                [one] => one.name.clone().into(),
                many => tn(many.len(), "{} item", "{} items").into(),
            };
            item = match side {
                Side::Local => item.on_drag(
                    DraggedFiles(dragged.iter().map(|row| PathBuf::from(&row.path)).collect()),
                    move |_, _, _, cx| cx.new(|_| DragLabel(label.clone())),
                ),
                Side::Remote => match &session {
                    Some(session) => item.on_drag(
                        DraggedRemote { session: session.clone(), paths: dragged.iter().map(|row| row.path.clone()).collect() },
                        move |_, _, _, cx| cx.new(|_| DragLabel(label.clone())),
                    ),
                    None => item,
                },
            };
        }
        if let Some(into) = into.filter(|_| !row.up) {
            item = match side {
                Side::Local => {
                    let into = into.clone();
                    item.drag_over::<DraggedRemote>(move |style, _, _, _| style.bg(drop_bg))
                        .on_drop(cx.listener(move |this, dragged: &DraggedRemote, _, cx| {
                            cx.stop_propagation();
                            this.drop_on_local(dragged, Some(into.clone()), cx)
                        }))
                }
                Side::Remote => {
                    let (a, b, c) = (into.clone(), into.clone(), into.clone());
                    item.drag_over::<DraggedFiles>(move |style, _, _, _| style.bg(drop_bg))
                        .drag_over::<DraggedRemote>(move |style, _, _, _| style.bg(drop_bg))
                        .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
                        .on_drop(cx.listener(move |this, files: &DraggedFiles, _, cx| {
                            cx.stop_propagation();
                            this.drop_on_remote(files.0.clone(), Some(a.clone()), cx)
                        }))
                        .on_drop(cx.listener(move |this, dragged: &DraggedRemote, _, cx| {
                            cx.stop_propagation();
                            this.drop_remote_on_remote(dragged, b.clone(), cx)
                        }))
                        .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                            cx.stop_propagation();
                            this.drop_external(paths.paths().to_vec(), Some(c.clone()), cx)
                        }))
                }
            };
        }
        let size = if row.is_dir { String::new() } else { format_size(row.size) };
        let modified = row.modified.map(format_time).unwrap_or_default();
        let permissions = format_permissions(row);

        div()
            .w_full()
            .px_1()
            .child(
                item.child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_1p5()
                        .child(ui::icon(icon).size_4().flex_shrink_0().text_color(icon_color))
                        .child(div().min_w_0().truncate().child(row.name.clone()))
                        .when(row.is_link, |name| name.child(ui::icon(IconName::Link).size_3().flex_shrink_0().text_color(muted))),
                )
                .child(div().flex_shrink_0().w(rems(SIZE_WIDTH)).flex().justify_end().text_color(muted).child(size))
                .child(div().flex_shrink_0().w(rems(MODIFIED_WIDTH)).truncate().text_color(muted).child(modified))
                .when(side == Side::Remote, |item| {
                    item.child(
                        div()
                            .flex_shrink_0()
                            .w(rems(PERMISSIONS_WIDTH))
                            .font_family("Menlo")
                            .text_size(rems(0.6875))
                            .text_color(muted)
                            .child(permissions),
                    )
                }),
            )
            .into_any_element()
    }

    fn render_list(&self, side: Side, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let pane = self.pane(side);
        let drop_bg = theme.tints.blue.base.opacity(0.08);
        let rows = pane.rows.len();
        let empty = pane.rows.iter().all(|row| row.up) && !pane.loading;
        let message = match (&pane.error, empty) {
            (Some(error), _) => Some((error.clone(), true)),
            (None, true) => Some((t("This folder is empty.").to_string(), false)),
            _ => None,
        };
        let list = uniform_list(
            SharedString::from(format!("sftp-rows-{side:?}")),
            rows,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let theme = Theme::global(cx).clone();
                range.map(|ix| this.render_row(side, ix, &theme, cx)).collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .py_1()
        .track_scroll(pane.scroll.clone());

        let mut area = div()
            .id(SharedString::from(format!("sftp-list-{side:?}")))
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0();
        // Dropping anywhere off the rows puts things in the folder shown.
        area = match side {
            Side::Local => area
                .drag_over::<DraggedRemote>(move |style, _, _, _| style.bg(drop_bg))
                .on_drop(cx.listener(|this, dragged: &DraggedRemote, _, cx| this.drop_on_local(dragged, None, cx))),
            Side::Remote => area
                .drag_over::<DraggedFiles>(move |style, _, _, _| style.bg(drop_bg))
                .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(drop_bg))
                .on_drop(cx.listener(|this, files: &DraggedFiles, _, cx| this.drop_on_remote(files.0.clone(), None, cx)))
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.drop_external(paths.paths().to_vec(), None, cx))),
        };
        area.when(side == Side::Remote && self.prompt.is_some(), |area| area.child(self.render_prompt(theme, cx)))
            .child(list)
            .when_some(message, |area, (message, error)| {
                area.child(
                    div()
                        .flex_shrink_0()
                        .px_4()
                        .py_3()
                        .text_center()
                        .text_size(rems(0.75))
                        .text_color(if error { theme.tints.destructive.base } else { theme.muted_foreground })
                        .child(message),
                )
            })
            .into_any_element()
    }

    fn render_prompt(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let Some(prompt) = &self.prompt else {
            return div();
        };
        let label = if prompt.renaming.is_some() { t("New name") } else { t("New folder") };
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .text_size(rems(0.75))
            .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(label))
            .child(div().flex_1().min_w_0().child(prompt.input.clone()))
            .child(
                ui::compact_button("sftp-prompt-ok", None, if prompt.renaming.is_some() { t("Rename") } else { t("Create") }, true, theme)
                    .on_click(cx.listener(|this, _, window, cx| this.submit_prompt(window, cx))),
            )
            .child(
                ui::compact_button("sftp-prompt-cancel", None, t("Cancel"), false, theme).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.prompt = None;
                        window.focus(&this.focus_handle);
                        cx.notify();
                    },
                )),
            )
    }

    /// The server side when there's no listing: what to do instead.
    fn render_remote_state(&self, theme: &Theme, cx: &Context<Self>) -> Option<AnyElement> {
        let card = || {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .text_center()
                .text_size(rems(0.8125))
        };
        let title = |text: String| div().font_weight(FontWeight::SEMIBOLD).text_color(theme.foreground).child(text);
        let body = |text: String| div().max_w(rems(26.)).text_color(theme.muted_foreground).child(text);
        Some(match &self.link {
            Link::Connected { .. } => return None,
            Link::Idle => {
                let sites = Sites::all(cx);
                if sites.is_empty() {
                    card()
                        .child(ui::icon(IconName::Server).size_8().text_color(theme.muted_foreground))
                        .child(title(t("No servers yet").into()))
                        .child(body(t("Add the servers you send files to: the address, the user, and how to sign in. Every project's SFTP windows can use them.").into()))
                        .child(
                            ui::compact_button("sftp-add-first", Some(IconName::Plus), t("Add a server"), true, theme)
                                .on_click(cx.listener(|this, _, window, cx| this.open_editor(None, window, cx))),
                        )
                        .into_any_element()
                } else {
                    card()
                        .child(title(t("Pick a server to connect to").into()))
                        .child(div().flex().flex_col().gap_1p5().w(rems(18.)).children(sites.into_iter().map(|site| {
                            let id = site.id.clone();
                            let hover = theme.secondary;
                            div()
                                .id(SharedString::from(format!("sftp-pick-{}", site.id)))
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_2()
                                .rounded(radius::SM)
                                .border_1()
                                .border_color(theme.border)
                                .cursor_pointer()
                                .hover(move |style| style.bg(hover))
                                .child(ui::icon(IconName::Server).size_4().text_color(theme.muted_foreground))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .min_w_0()
                                        .items_start()
                                        .child(div().truncate().text_color(theme.foreground).child(site.label()))
                                        .child(div().truncate().text_size(rems(0.6875)).text_color(theme.muted_foreground).child(site.site.address())),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.reconnects = 0;
                                    this.connect(id.clone(), None, cx)
                                }))
                        })))
                        .child(
                            ui::compact_button("sftp-add", Some(IconName::Plus), t("Add a server"), false, theme)
                                .on_click(cx.listener(|this, _, window, cx| this.open_editor(None, window, cx))),
                        )
                        .into_any_element()
                }
            }
            Link::Connecting { site } => {
                let name = Sites::get(site, cx).map(|s| s.label()).unwrap_or_default();
                card().child(body(tf("Connecting to {}…", &[&name]))).into_any_element()
            }
            Link::Trust { site, fingerprint } => {
                let saved = Sites::get(site, cx);
                let host = saved.as_ref().map(|s| s.site.host.clone()).unwrap_or_default();
                let (id, fingerprint) = (site.clone(), fingerprint.clone());
                card()
                    .child(ui::icon(IconName::KeyRound).size_8().text_color(theme.tints.amber.base))
                    .child(title(tf("First time connecting to {}", &[&host])))
                    .child(body(t("Make sure this is its key before you trust it. Its fingerprint:").into()))
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(radius::XS)
                            .bg(theme.muted)
                            .font_family("Menlo")
                            .text_size(rems(0.75))
                            .text_color(theme.foreground)
                            .child(fingerprint.clone()),
                    )
                    .child(body(t("Trusting it adds it to ~/.ssh/known_hosts, so you won't be asked again.").into()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                ui::compact_button("sftp-trust", Some(IconName::Check), t("Trust and connect"), true, theme)
                                    .on_click(cx.listener(move |this, _, _, cx| this.connect(id.clone(), Some(fingerprint.clone()), cx))),
                            )
                            .child(ui::compact_button("sftp-distrust", None, t("Cancel"), false, theme).on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.disconnect(cx);
                                    this.settings.site = None;
                                    this.emit_changed(cx);
                                },
                            ))),
                    )
                    .into_any_element()
            }
            Link::Failed { site, error } => {
                let (retry, edit) = (site.clone(), site.clone());
                card()
                    .child(ui::icon(IconName::TriangleAlert).size_8().text_color(theme.tints.destructive.base))
                    .child(title(t("Couldn't connect").into()))
                    .child(body(error.clone()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                ui::compact_button("sftp-retry", Some(IconName::RotateCcw), t("Try again"), true, theme)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.reconnects = 0;
                                        this.connect(retry.clone(), None, cx)
                                    })),
                            )
                            .child(
                                ui::compact_button("sftp-edit", Some(IconName::Pencil), t("Edit server"), false, theme)
                                    .on_click(cx.listener(move |this, _, window, cx| this.open_editor(Some(edit.clone()), window, cx))),
                            ),
                    )
                    .into_any_element()
            }
        })
    }

    fn render_pane(&self, side: Side, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let remote_state = (side == Side::Remote).then(|| self.render_remote_state(theme, cx)).flatten();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .when(side == Side::Remote, |pane| pane.border_l_1().border_color(theme.border))
            .child(self.render_path_bar(side, theme, cx))
            .map(|pane| match remote_state {
                Some(state) => pane.child(state),
                None => pane.child(self.render_header(side, theme, cx)).child(self.render_list(side, theme, cx)),
            })
    }

    fn render_transfers(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let muted = theme.muted_foreground;
        let finished = self.transfers.iter().filter(|t| t.state.finished()).count();
        let bar = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .h_7()
            .px_3()
            .border_t_1()
            .border_color(theme.border)
            .text_size(rems(0.6875))
            .text_color(muted);
        if self.transfers.is_empty() {
            return bar
                .child(t("Drag files from one side to the other to copy them, or double-click a file to send it across."))
                .into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .max_h(rems(10.))
            .child(
                bar.child(div().flex_1().font_weight(FontWeight::MEDIUM).text_color(theme.foreground).child(tn(
                    self.transfers.len(),
                    "{} transfer",
                    "{} transfers",
                )))
                .when(finished > 0, |bar| {
                    bar.child(
                        ui::compact_button("sftp-clear", None, t("Clear finished"), false, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.clear_finished(cx))),
                    )
                }),
            )
            .child(
                div()
                    .id("sftp-transfers")
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .px_3()
                    .pb_1()
                    .children(self.transfers.iter().rev().map(|transfer| self.render_transfer(transfer, theme, cx))),
            )
            .into_any_element()
    }

    fn render_transfer(&self, transfer: &Transfer, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let muted = theme.muted_foreground;
        let (icon, arrow) = match transfer.direction {
            Direction::Upload => (IconName::Upload, "→"),
            Direction::Download => (IconName::Download, "←"),
        };
        let name = if transfer.items > 1 {
            tf("{} and {} more", &[&transfer.name, &(transfer.items - 1)])
        } else {
            transfer.name.clone()
        };
        let into = match transfer.direction {
            Direction::Upload => transfer.into.clone(),
            Direction::Download => ui::display_path(Path::new(&transfer.into)),
        };
        let fraction = if transfer.bytes_total == 0 {
            if transfer.state == TransferState::Done { 1. } else { 0. }
        } else {
            transfer.bytes_done as f32 / transfer.bytes_total as f32
        };
        let (status, color) = match &transfer.state {
            TransferState::Preparing => (t("Getting ready…").to_string(), muted),
            TransferState::Running => (
                tf(
                    "{} of {} files · {} of {}",
                    &[&transfer.files_done, &transfer.files_total, &format_size(transfer.bytes_done), &format_size(transfer.bytes_total)],
                ),
                muted,
            ),
            TransferState::Done => (tn(transfer.files_total, "{} file copied", "{} files copied"), theme.tints.green.base),
            TransferState::Failed(error) => (error.clone(), theme.tints.destructive.base),
            TransferState::Cancelled => (t("Cancelled").to_string(), muted),
        };
        let running = !transfer.state.finished();
        let id = transfer.id;
        div()
            .flex()
            .items_center()
            .gap_2()
            .h_7()
            .text_size(rems(0.75))
            .child(ui::icon(icon).size_3p5().flex_shrink_0().text_color(muted))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(div().flex_shrink().min_w_0().truncate().text_color(theme.foreground).child(name))
                    .child(div().flex_shrink_0().text_color(muted).child(arrow))
                    .child(div().flex_shrink().min_w_0().truncate().text_color(muted).child(into)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(rems(6.))
                    .h(px(4.))
                    .rounded_full()
                    .bg(theme.muted)
                    .child(div().h_full().rounded_full().w(relative(fraction.clamp(0., 1.))).bg(if transfer.state == TransferState::Done {
                        theme.tints.green.base
                    } else {
                        theme.tints.blue.base
                    })),
            )
            .child(div().flex_shrink_0().w(rems(13.)).truncate().text_color(color).child(status))
            .child(if running {
                ui::small_icon_button(SharedString::from(format!("sftp-cancel-{id}")), IconName::X, theme)
                    .on_click(cx.listener(move |this, _, _, cx| this.cancel_transfer(id, cx)))
                    .into_any_element()
            } else {
                div().size_6().flex_shrink_0().into_any_element()
            })
            .into_any_element()
    }

    fn render_editor(&self, editor: &SiteEditor, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let sites = Sites::all(cx);
        let hover = theme.secondary;
        let field = |label: &'static str, control: AnyElement| {
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(div().text_size(rems(0.75)).font_weight(FontWeight::MEDIUM).text_color(theme.foreground).child(t(label)))
                .child(control)
        };
        let input = |entity: &Entity<TextInput>| div().text_size(rems(0.8125)).child(entity.clone()).into_any_element();
        let editing = editor.id.clone();
        let auth = editor.auth;
        let segment = |label: &'static str, kind: AuthKind| {
            Segment::new(t(label), auth == kind, cx.listener(move |this, _, _, cx| {
                if let Some(editor) = &mut this.editor {
                    editor.auth = kind;
                    cx.notify();
                }
            }))
        };
        let on_remote = self.server.is_some();
        div()
            .id("sftp-editor")
            .absolute()
            .inset_0()
            .flex()
            .bg(theme.background)
            .text_size(rems(0.8125))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            // The list of servers.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .w(rems(14.))
                    .border_r_1()
                    .border_color(theme.border)
                    .bg(theme.card)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .h_10()
                            .px_3()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(t("Servers")),
                    )
                    .child(
                        div()
                            .id("sftp-editor-sites")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_1p5()
                            .gap_0p5()
                            .children(sites.iter().map(|site| {
                                let picked = editing.as_deref() == Some(site.id.as_str());
                                let id = site.id.clone();
                                div()
                                    .id(SharedString::from(format!("sftp-editor-{}", site.id)))
                                    .flex()
                                    .flex_col()
                                    .px_2()
                                    .py_1p5()
                                    .rounded(radius::SM)
                                    .cursor_pointer()
                                    .map(|row| if picked { row.bg(theme.tints.blue.soft) } else { row.hover(move |style| style.bg(hover)) })
                                    .child(div().truncate().text_color(theme.foreground).child(site.label()))
                                    .child(div().truncate().text_size(rems(0.6875)).text_color(theme.muted_foreground).child(site.site.address()))
                                    .on_click(cx.listener(move |this, _, window, cx| this.open_editor(Some(id.clone()), window, cx)))
                            })),
                    )
                    .child(
                        div().p_2().border_t_1().border_color(theme.border).child(
                            ui::compact_button("sftp-editor-new", Some(IconName::Plus), t("New server"), false, theme)
                                .on_click(cx.listener(|this, _, window, cx| this.open_editor(None, window, cx))),
                        ),
                    ),
            )
            // The form.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .id("sftp-editor-form")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .gap_4()
                            .p_5()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(rems(0.9375))
                                    .text_color(theme.foreground)
                                    .child(if editing.is_some() { t("Edit server") } else { t("New server") }),
                            )
                            .child(field("Name", input(&editor.name)))
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .child(div().flex_1().child(field("Host", input(&editor.host))))
                                    .child(div().w(rems(6.)).child(field("Port", input(&editor.port)))),
                            )
                            .child(field("User", input(&editor.user)))
                            .child(field(
                                "Sign in with",
                                ui::segmented(
                                    "sftp-auth",
                                    vec![
                                        segment("SSH keys", AuthKind::Agent),
                                        segment("Password", AuthKind::Password),
                                        segment("Key file", AuthKind::KeyFile),
                                    ],
                                    theme,
                                )
                                .into_any_element(),
                            ))
                            .map(|form| match auth {
                                AuthKind::Agent => form.child(
                                    div().text_size(rems(0.75)).text_color(theme.muted_foreground).child(if on_remote {
                                        t("The SSH agent's keys, then ~/.ssh/id_ed25519, id_ecdsa or id_rsa, on this project's server (it's what connects).")
                                    } else {
                                        t("The SSH agent's keys, then ~/.ssh/id_ed25519, id_ecdsa or id_rsa.")
                                    }),
                                ),
                                AuthKind::Password => form.child(field("Password", input(&editor.password))).child(
                                    div()
                                        .text_size(rems(0.75))
                                        .text_color(theme.muted_foreground)
                                        .child(t("Saved on this Mac in a file only you can read.")),
                                ),
                                AuthKind::KeyFile => form
                                    .child(field("Key file", input(&editor.key_path)))
                                    .child(field("Passphrase", input(&editor.passphrase)))
                                    .when(on_remote, |form| {
                                        form.child(
                                            div()
                                                .text_size(rems(0.75))
                                                .text_color(theme.muted_foreground)
                                                .child(t("The key file is looked for on this project's server (it's what connects).")),
                                        )
                                    }),
                            })
                            .child(field("Folder to open", input(&editor.remote_dir)))
                            .when_some(editor.error.clone(), |form, error| {
                                form.child(div().text_size(rems(0.75)).text_color(theme.tints.destructive.base).child(error))
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_2()
                            .px_5()
                            .py_3()
                            .border_t_1()
                            .border_color(theme.border)
                            .when_some(editing.clone(), |bar, id| {
                                bar.child(
                                    ui::compact_button("sftp-editor-remove", Some(IconName::Trash), t("Remove"), false, theme)
                                        .text_color(theme.tints.destructive.base)
                                        .on_click(cx.listener(move |this, _, _, cx| this.confirm_remove_site(id.clone(), cx))),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                ui::compact_button("sftp-editor-close", None, t("Close"), false, theme)
                                    .on_click(cx.listener(|this, _, window, cx| this.close_editor(window, cx))),
                            )
                            .child(
                                ui::compact_button("sftp-editor-save", None, t("Save"), false, theme)
                                    .on_click(cx.listener(|this, _, window, cx| this.save_editor(false, window, cx))),
                            )
                            .child(
                                ui::compact_button("sftp-editor-connect", Some(IconName::Plug), t("Save and connect"), true, theme)
                                    .on_click(cx.listener(|this, _, window, cx| this.save_editor(true, window, cx))),
                            ),
                    ),
            )
    }
}

impl Render for SftpView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx).clone();
        div()
            .id("sftp")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .text_size(rems(0.8125))
            .line_height(rems(1.25))
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            // The lists scroll; the canvas shouldn't pan along with them.
            .on_scroll_wheel(cx.listener(|this, _, window, cx| {
                if this.contains_focus(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(self.render_toolbar(&theme, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_pane(Side::Local, &theme, cx))
                    .child(self.render_pane(Side::Remote, &theme, cx)),
            )
            .child(self.render_transfers(&theme, cx))
            .when_some(self.editor.as_ref(), |view, editor| view.child(self.render_editor(editor, &theme, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, is_dir: bool, size: u64, modified: u64) -> Row {
        Row {
            name: name.into(),
            path: format!("/{name}"),
            is_dir,
            is_link: false,
            size,
            modified: Some(modified),
            permissions: Some(0o644),
            up: false,
        }
    }

    #[test]
    fn folders_come_first_then_the_column() {
        let mut rows = vec![row("b10.txt", false, 5, 1), row("logs", true, 0, 9), row("b2.txt", false, 50, 3)];
        rows.sort_by(|a, b| compare(a, b, SortBy::Name, false));
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["logs", "b2.txt", "b10.txt"]);
        rows.sort_by(|a, b| compare(a, b, SortBy::Size, true));
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["logs", "b2.txt", "b10.txt"]);
        rows.sort_by(|a, b| compare(a, b, SortBy::Modified, false));
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["logs", "b10.txt", "b2.txt"]);
    }

    #[test]
    fn modes_read_like_ls() {
        let mut file = row("a", false, 0, 0);
        file.permissions = Some(0o640);
        assert_eq!(format_permissions(&file), "-rw-r-----");
        let mut dir = row("d", true, 0, 0);
        dir.permissions = Some(0o755);
        assert_eq!(format_permissions(&dir), "drwxr-xr-x");
    }

    #[test]
    fn tilde_is_the_servers_home() {
        assert_eq!(expand_remote("~", "/home/me"), "/home/me");
        assert_eq!(expand_remote("~/logs", "/home/me"), "/home/me/logs");
        assert_eq!(expand_remote("/var/www", "/home/me"), "/var/www");
    }
}
