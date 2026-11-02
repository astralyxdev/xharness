//! The editor window: a text file, editable, with syntax highlighting.
//!
//! - The file is a list of lines; positions are (line, byte column).
//! - Typing (and IME composition) arrives through the input handler; every
//!   other key is an action bound in the `Editor` context (macOS keys: ⌥
//!   moves by word, ⌘ by line or file, ⌘Z/⇧⌘Z undo and redo, ⌘S saves).
//! - Highlighting runs in the background, a chunk of lines at a time,
//!   starting at the first line an edit touched and stopping as soon as the
//!   grammar's state matches what it was before (see `windows/editor/syntax.rs`).
//! - The file is watched: changes on disk (an agent's, git's) are taken in
//!   at once if there are no unsaved edits, as an edit you can undo;
//!   otherwise a banner offers to reload.
//!
//! Everything is sized from the rem size, so the canvas zooms it.

mod completion;
mod element;
mod file;
pub mod syntax;
mod text;

use completion::*;
use element::*;
use file::*;
use text::*;

use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Font, FontWeight, GlobalElementId,
    Hsla, IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, PathBuilder, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString, Size, Style, Subscription,
    Task, TextRun, UTF16Selection, Window, actions, anchored, deferred, div, fill, font, point, prelude::*, px,
    relative, rems, size,
};
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;

use crate::i18n::{t, tf, tn};
use crate::ui::assets::IconName;
use crate::windows::explorer::open_with_default_app;
use crate::services::lsp::{self, Attached, Lsp};
use crate::services::server::HostAddress;
use syntax::{Language, Span};
use crate::ui::theme::{FONT_MONO, Theme};
use crate::ui;

actions!(
    editor,
    [
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        MoveWordLeft,
        MoveWordRight,
        SelectWordLeft,
        SelectWordRight,
        MoveLineStart,
        MoveLineEnd,
        SelectLineStart,
        SelectLineEnd,
        MoveFileStart,
        MoveFileEnd,
        SelectFileStart,
        SelectFileEnd,
        PageUp,
        PageDown,
        SelectPageUp,
        SelectPageDown,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        DeleteToLineEnd,
        DeleteLine,
        Newline,
        NewlineBelow,
        Indent,
        Outdent,
        ToggleComment,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Save,
        Escape,
        ShowCharacterPalette,
        GoToDefinition,
        ShowCompletions,
    ]
);

/// What an editor asks of the canvas.
pub enum EditorEvent {
    /// Open `path` at `line` and UTF-16 `character` (go to definition in
    /// another file).
    OpenAt { path: PathBuf, line: usize, character: usize },
}

const KEY_CONTEXT: &str = "Editor";
/// At 100% zoom.
const FONT_SIZE: f32 = 13.;
const LINE_HEIGHT: f32 = 20.;
/// Columns a tab takes on screen.
const TAB_WIDTH: usize = 4;
/// Files bigger than this, or with a line longer than `MAX_HIGHLIGHT_LINE`,
/// aren't highlighted (grammars get slow on minified files).
const MAX_HIGHLIGHT_BYTES: usize = 4 * 1024 * 1024;
const MAX_HIGHLIGHT_LINE: usize = 10_000;
/// Lines highlighted per background job.
const CHUNK: usize = 400;
const WATCH_EVERY: Duration = Duration::from_millis(1500);
/// Typing within this long of the last keystroke undoes together.
const TYPING_GROUP: Duration = Duration::from_secs(1);

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("left", MoveLeft, context),
        KeyBinding::new("right", MoveRight, context),
        KeyBinding::new("up", MoveUp, context),
        KeyBinding::new("down", MoveDown, context),
        KeyBinding::new("shift-left", SelectLeft, context),
        KeyBinding::new("shift-right", SelectRight, context),
        KeyBinding::new("shift-up", SelectUp, context),
        KeyBinding::new("shift-down", SelectDown, context),
        KeyBinding::new("alt-left", MoveWordLeft, context),
        KeyBinding::new("alt-right", MoveWordRight, context),
        KeyBinding::new("alt-shift-left", SelectWordLeft, context),
        KeyBinding::new("alt-shift-right", SelectWordRight, context),
        KeyBinding::new("secondary-left", MoveLineStart, context),
        KeyBinding::new("secondary-right", MoveLineEnd, context),
        KeyBinding::new("home", MoveLineStart, context),
        KeyBinding::new("end", MoveLineEnd, context),
        KeyBinding::new("secondary-shift-left", SelectLineStart, context),
        KeyBinding::new("secondary-shift-right", SelectLineEnd, context),
        KeyBinding::new("shift-home", SelectLineStart, context),
        KeyBinding::new("shift-end", SelectLineEnd, context),
        KeyBinding::new("secondary-up", MoveFileStart, context),
        KeyBinding::new("secondary-down", MoveFileEnd, context),
        KeyBinding::new("secondary-shift-up", SelectFileStart, context),
        KeyBinding::new("secondary-shift-down", SelectFileEnd, context),
        KeyBinding::new("pageup", PageUp, context),
        KeyBinding::new("pagedown", PageDown, context),
        KeyBinding::new("shift-pageup", SelectPageUp, context),
        KeyBinding::new("shift-pagedown", SelectPageDown, context),
        KeyBinding::new("backspace", Backspace, context),
        KeyBinding::new("shift-backspace", Backspace, context),
        KeyBinding::new("delete", Delete, context),
        KeyBinding::new("alt-backspace", DeleteWordLeft, context),
        KeyBinding::new("alt-delete", DeleteWordRight, context),
        KeyBinding::new("secondary-backspace", DeleteToLineStart, context),
        KeyBinding::new("secondary-shift-k", DeleteLine, context),
        KeyBinding::new("enter", Newline, context),
        KeyBinding::new("shift-enter", Newline, context),
        KeyBinding::new("secondary-enter", NewlineBelow, context),
        KeyBinding::new("tab", Indent, context),
        KeyBinding::new("shift-tab", Outdent, context),
        KeyBinding::new("secondary-]", Indent, context),
        KeyBinding::new("secondary-[", Outdent, context),
        KeyBinding::new("secondary-/", ToggleComment, context),
        KeyBinding::new("secondary-a", SelectAll, context),
        KeyBinding::new("secondary-c", Copy, context),
        KeyBinding::new("secondary-x", Cut, context),
        KeyBinding::new("secondary-v", Paste, context),
        KeyBinding::new("secondary-z", Undo, context),
        KeyBinding::new("secondary-shift-z", Redo, context),
        KeyBinding::new("secondary-s", Save, context),
        KeyBinding::new("escape", Escape, context),
        KeyBinding::new("f12", GoToDefinition, context),
        KeyBinding::new("ctrl-space", ShowCompletions, context),
    ]);

    // macOS text fields' Emacs keys; elsewhere Ctrl is the shortcut key.
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("ctrl-b", MoveLeft, context),
        KeyBinding::new("ctrl-f", MoveRight, context),
        KeyBinding::new("ctrl-p", MoveUp, context),
        KeyBinding::new("ctrl-n", MoveDown, context),
        KeyBinding::new("ctrl-a", MoveLineStart, context),
        KeyBinding::new("ctrl-e", MoveLineEnd, context),
        KeyBinding::new("ctrl-h", Backspace, context),
        KeyBinding::new("ctrl-d", Delete, context),
        KeyBinding::new("ctrl-k", DeleteToLineEnd, context),
        KeyBinding::new("ctrl-secondary-space", ShowCharacterPalette, context),
    ]);
}

// Undo ---------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Typing,
    Other,
}

/// One undoable change: `old` at `start` became `new`.
struct Edit {
    /// Identifies the text this edit leaves, for telling saved from not.
    id: u64,
    start: Pos,
    old: String,
    new: String,
    before: Selection,
    after: Selection,
    kind: EditKind,
    at: Instant,
}

// Highlighting -------------------------------------------------------------------------

struct LineSyntax {
    /// The grammar's state where the line starts (`None` until known).
    start: Option<syntax::State>,
    spans: Vec<Span>,
    /// Needs highlighting again (its spans may be out of date).
    stale: bool,
}

impl LineSyntax {
    fn stale() -> Self {
        Self {
            start: None,
            spans: Vec::new(),
            stale: true,
        }
    }
}

struct Highlighting {
    language: Language,
    lines: Vec<LineSyntax>,
    task: Option<Task<()>>,
}

// The view -----------------------------------------------------------------------------

enum Doc {
    Loading,
    Ready,
    Failed(String),
}

/// The file changed under us.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Conflict {
    /// Changed on disk while there are unsaved edits.
    Changed,
    Deleted,
}

/// Where things were drawn last frame, for the mouse and the IME.
#[derive(Clone, Copy)]
struct Layout {
    /// Where column 0 of line 0 is (scrolling moves it).
    origin: Point<Pixels>,
    line_height: Pixels,
    char_width: Pixels,
}

pub struct EditorView {
    path: PathBuf,
    /// The server the file is on (`None`: this machine).
    server: Option<HostAddress>,
    doc: Doc,
    buffer: Buffer,
    crlf: bool,
    indent: IndentStyle,
    selection: Selection,
    /// The screen column up/down keeps to.
    goal_column: Option<usize>,
    /// IME composition, as a byte range in the caret's line.
    marked: Option<Range<usize>>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    next_id: u64,
    saved_id: u64,
    disk: Option<Stamp>,
    conflict: Option<Conflict>,
    error: Option<String>,
    syntax: Option<Highlighting>,
    /// Bumped on every change, so highlighting knows when it's out of date.
    version: u64,
    /// Scroll position: lines from the top, and px (at 100%) from the left.
    scroll_top: f32,
    scroll_left: f32,
    /// Bring the caret into view at the next paint.
    autoscroll: bool,
    /// Rows that fit, from the last paint.
    rows: usize,
    layout: Option<Layout>,
    /// The visible lines as last shaped, by line number.
    shaped: Vec<(usize, Option<ShapedLine>)>,
    max_columns: Option<usize>,
    selecting: bool,
    /// What ⌘C copied without a selection (a whole line), so pasting it
    /// goes above the caret's line, as in other editors.
    line_clipboard: Option<String>,
    /// The language server for this file, if one's installed.
    lsp: Option<Attached>,
    /// The file as the server knows it (its real path, as a URI).
    lsp_path: PathBuf,
    lsp_uri: String,
    lsp_version: i32,
    lsp_generation: u64,
    /// Edits the server hasn't been told about yet.
    unsynced: bool,
    /// Those edits, for servers that take them one by one (`None` until
    /// the server has the file).
    pending_changes: Option<Vec<Value>>,
    sync_task: Option<Task<()>>,
    diagnostics: Vec<lsp::Diagnostic>,
    diagnostics_version: u64,
    completion: Option<Completion>,
    completion_task: Option<Task<()>>,
    /// The caret's bottom-left in the window, from the last paint (where
    /// completions open).
    caret_point: Option<Point<Pixels>>,
    /// Where to put the caret once the file has loaded.
    pending_goto: Option<(usize, usize)>,
    _lsp: Option<Subscription>,
    _release: Option<Subscription>,
    focus_handle: FocusHandle,
    _load: Option<Task<()>>,
    _watch: Task<()>,
}

impl EditorView {
    pub fn new(path: PathBuf, server: Option<HostAddress>, cx: &mut Context<Self>) -> Self {
        let watch = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(WATCH_EVERY).await;
                let Ok((path, server)) = this.read_with(cx, |this, _| (this.path.clone(), this.server.clone())) else {
                    break;
                };
                let stamp = cx.background_executor().spawn(async move { crate::services::rpc::on(server.as_ref(), || stamp_of(&path)) }).await;
                if this.update(cx, |this, cx| this.check_disk(stamp, cx)).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            path,
            server,
            doc: Doc::Loading,
            buffer: Buffer::new(""),
            crlf: false,
            indent: IndentStyle::Spaces(4),
            selection: Selection::default(),
            goal_column: None,
            marked: None,
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            saved_id: 0,
            disk: None,
            conflict: None,
            error: None,
            syntax: None,
            version: 0,
            scroll_top: 0.,
            scroll_left: 0.,
            autoscroll: false,
            rows: 20,
            layout: None,
            shaped: Vec::new(),
            max_columns: None,
            selecting: false,
            line_clipboard: None,
            lsp: None,
            lsp_path: PathBuf::new(),
            lsp_uri: String::new(),
            lsp_version: 0,
            lsp_generation: 0,
            unsynced: false,
            pending_changes: None,
            sync_task: None,
            diagnostics: Vec::new(),
            diagnostics_version: 0,
            completion: None,
            completion_task: None,
            caret_point: None,
            pending_goto: None,
            _lsp: None,
            _release: None,
            focus_handle: cx.focus_handle(),
            _load: None,
            _watch: watch,
        };
        // Tell the server the file's closed when the window goes.
        this._release = Some(cx.on_release(|this: &mut Self, _: &mut App| {
            if let Some(attached) = &this.lsp {
                attached.client.notify("textDocument/didClose", json!({ "textDocument": { "uri": this.lsp_uri } }));
            }
        }));
        this.load(cx);
        this
    }

    // Language servers -----------------------------------------------------------------

    /// Finds and starts this file's language server, if one's installed.
    fn attach_lsp(&mut self, cx: &mut Context<Self>) {
        // Language servers run in the app, on this machine's files.
        if self.server.is_some() || self.lsp.is_some() || !matches!(self.doc, Doc::Ready) {
            return;
        }
        let Some(language) = Language::detect(&self.path, self.buffer.line(0)).map(|l| l.name()) else {
            return;
        };
        let store = Lsp::global(cx);
        self.lsp_generation = store.read(cx).generation();
        if self._lsp.is_none() {
            self._lsp = Some(cx.observe(&store, |this, store, cx| this.lsp_changed(store, cx)));
        }
        let path = lsp::canonical(&self.path);
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { crate::services::language_servers::launcher_for(language) })
                .await;
            let Some(found) = found else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.lsp.is_some() {
                    return;
                }
                let Some(attached) = store.update(cx, |store, cx| store.attach(&path, found, cx)) else {
                    return;
                };
                this.lsp_uri = lsp::path_to_uri(&path);
                this.lsp_path = path;
                this.lsp_version = 1;
                attached.client.notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": {
                        "uri": this.lsp_uri,
                        "languageId": attached.language_id,
                        "version": 1,
                        "text": this.buffer.text("\n"),
                    }}),
                );
                this.lsp = Some(attached);
                this.unsynced = false;
                this.pending_changes = Some(Vec::new());
                this.lsp_changed(store, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn lsp_changed(&mut self, store: Entity<Lsp>, cx: &mut Context<Self>) {
        let (generation, diagnostics) = {
            let store = store.read(cx);
            (store.generation(), store.diagnostics(&self.lsp_path).map(|(v, d)| (v, d.to_vec())))
        };
        if let Some((version, items)) = diagnostics
            && version != self.diagnostics_version
        {
            self.diagnostics_version = version;
            self.diagnostics = items;
            cx.notify();
        }
        // A server was installed: try again.
        if self.lsp.is_none() && generation != self.lsp_generation {
            self.attach_lsp(cx);
        }
    }

    /// Tells the server about edits a moment after they stop.
    fn schedule_sync(&mut self, cx: &mut Context<Self>) {
        if self.lsp.is_none() {
            return;
        }
        self.unsynced = true;
        self.sync_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            this.update(cx, |this, _| this.sync_now()).ok();
        }));
    }

    fn sync_now(&mut self) {
        if !self.unsynced {
            return;
        }
        let Some(attached) = &self.lsp else {
            return;
        };
        self.unsynced = false;
        self.lsp_version += 1;
        // The edits since last time, or the whole text.
        let changes = match self.pending_changes.replace(Vec::new()) {
            Some(changes) if attached.client.incremental_sync() && !changes.is_empty() => Value::Array(changes),
            _ => json!([{ "text": self.buffer.text("\n") }]),
        };
        attached.client.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": self.lsp_uri, "version": self.lsp_version },
                "contentChanges": changes,
            }),
        );
    }

    fn lsp_position(&self, pos: Pos) -> Value {
        json!({ "line": pos.line, "character": byte_to_utf16(self.buffer.line(pos.line), pos.col) })
    }

    fn request_completions(&mut self, cx: &mut Context<Self>) {
        self.sync_now();
        let Some(attached) = &self.lsp else {
            return;
        };
        let head = self.selection.head;
        let line = self.buffer.line(head.line);
        let start = line[..head.col]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map_or(head.col, |(ix, _)| ix);
        let start = Pos::new(head.line, start);
        let reply = attached.client.request(
            "textDocument/completion",
            json!({ "textDocument": { "uri": self.lsp_uri }, "position": self.lsp_position(head) }),
        );
        self.completion_task = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(result)) = reply.await else {
                return;
            };
            this.update(cx, |this, cx| {
                let items = parse_completions(&result);
                let head = this.selection.head;
                if items.is_empty() || head.line != start.line || head.col < start.col {
                    this.completion = None;
                } else {
                    this.completion = Some(Completion { items, shown: Vec::new(), selected: 0, start });
                    this.filter_completions();
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Narrows the list to what's typed since it opened.
    fn filter_completions(&mut self) {
        let head = self.selection.head;
        let Some(completion) = &mut self.completion else {
            return;
        };
        if head.line != completion.start.line || head.col < completion.start.col {
            self.completion = None;
            return;
        }
        let typed = &self.buffer.line(head.line)[completion.start.col..head.col];
        let mut shown: Vec<(u8, usize)> = completion
            .items
            .iter()
            .enumerate()
            .filter_map(|(ix, item)| completion_rank(&item.filter, typed).map(|rank| (rank, ix)))
            .collect();
        shown.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| completion.items[a.1].sort.cmp(&completion.items[b.1].sort)));
        completion.shown = shown.into_iter().map(|(_, ix)| ix).take(200).collect();
        completion.selected = 0;
        if completion.shown.is_empty() {
            self.completion = None;
        }
    }

    /// After typing `text`: open or narrow completions.
    fn after_typing(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(attached) = &self.lsp else {
            return;
        };
        let Some(last) = text.chars().last() else {
            return;
        };
        let word = last.is_alphanumeric() || last == '_';
        let trigger = attached.client.completion_triggers().iter().any(|t| t.ends_with(last));
        if word && self.completion.is_some() {
            self.filter_completions();
            cx.notify();
        } else if word || trigger {
            self.request_completions(cx);
        } else {
            self.completion = None;
        }
    }

    fn accept_completion(&mut self, cx: &mut Context<Self>) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(item) = completion.shown.get(completion.selected).map(|&ix| &completion.items[ix]) else {
            return;
        };
        let head = self.selection.head;
        let line = self.buffer.line(head.line);
        let (range, text) = match &item.edit {
            // The edit's range is from when the list opened; what's been
            // typed since goes too.
            Some((line_ix, start, end, text)) if *line_ix == head.line => {
                let end = utf16_to_byte(line, *end).max(head.col);
                (Pos::new(head.line, utf16_to_byte(line, *start))..Pos::new(head.line, end), text.clone())
            }
            _ => (completion.start..head, item.insert.clone()),
        };
        self.edit(range, &text, None, EditKind::Other, cx);
    }

    fn move_completion(&mut self, by: isize, cx: &mut Context<Self>) {
        if let Some(completion) = &mut self.completion {
            let len = completion.shown.len() as isize;
            completion.selected = ((completion.selected as isize + by).rem_euclid(len)) as usize;
            cx.notify();
        }
    }

    fn go_to_definition(&mut self, at: Pos, cx: &mut Context<Self>) {
        self.sync_now();
        let Some(attached) = &self.lsp else {
            return;
        };
        let reply = attached.client.request(
            "textDocument/definition",
            json!({ "textDocument": { "uri": self.lsp_uri }, "position": self.lsp_position(at) }),
        );
        let here = self.lsp_path.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(result)) = reply.await else {
                return;
            };
            let location = match &result {
                Value::Array(items) => items.first().cloned(),
                Value::Null => None,
                other => Some(other.clone()),
            };
            let Some(location) = location else {
                return;
            };
            let uri = location["uri"].as_str().or(location["targetUri"].as_str());
            let range = location.get("targetSelectionRange").unwrap_or(&location["range"]);
            let Some(path) = uri.and_then(lsp::uri_to_path) else {
                return;
            };
            let line = range["start"]["line"].as_u64().unwrap_or(0) as usize;
            let character = range["start"]["character"].as_u64().unwrap_or(0) as usize;
            this.update(cx, |this, cx| {
                if path == here {
                    this.go_to(line, character, cx);
                } else {
                    cx.emit(EditorEvent::OpenAt { path, line, character });
                }
            })
            .ok();
        })
        .detach();
    }

    /// Puts the caret at `line` and UTF-16 `character` (once loaded).
    pub fn go_to(&mut self, line: usize, character: usize, cx: &mut Context<Self>) {
        if !matches!(self.doc, Doc::Ready) {
            self.pending_goto = Some((line, character));
            return;
        }
        let line = line.min(self.buffer.len() - 1);
        let col = utf16_to_byte(self.buffer.line(line), character);
        self.selection = Selection::cursor(Pos::new(line, col));
        // In the middle of the view, not at its edge.
        self.scroll_top = (line as f32 - self.rows as f32 / 3.).max(0.);
        self.autoscroll = true;
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether there are edits that aren't saved.
    pub fn dirty(&self) -> bool {
        matches!(self.doc, Doc::Ready) && self.state_id() != self.saved_id
    }

    /// A few words for the window's header: the language, and whether
    /// there are unsaved edits.
    pub fn summary(&self) -> Option<String> {
        if !matches!(self.doc, Doc::Ready) {
            return None;
        }
        let language = self.syntax.as_ref().map_or(t("Plain Text"), |h| h.language.name());
        Some(if self.dirty() { tf("{} · unsaved", &[&language]) } else { language.to_string() })
    }

    fn state_id(&self) -> u64 {
        self.undo.last().map_or(0, |edit| edit.id)
    }

    // Loading and saving -------------------------------------------------------------

    fn load(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let server = self.server.clone();
        self._load = Some(cx.spawn(async move |this, cx| {
            let loaded = cx.background_executor().spawn(async move { crate::services::rpc::on(server.as_ref(), || read_file(&path)) }).await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok(loaded) => this.open(loaded),
                    Err(error) => this.doc = Doc::Failed(error),
                }
                this.schedule_highlight(cx);
                this.attach_lsp(cx);
                if let Some((line, character)) = this.pending_goto.take() {
                    this.go_to(line, character, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open(&mut self, loaded: Loaded) {
        self.buffer = Buffer::new(&loaded.text);
        self.crlf = loaded.crlf;
        self.indent = IndentStyle::detect(&self.buffer.lines);
        self.disk = Some(loaded.stamp);
        self.doc = Doc::Ready;
        self.selection = Selection::default();
        self.undo.clear();
        self.redo.clear();
        self.saved_id = 0;
        self.max_columns = None;
        let highlightable = loaded.text.len() <= MAX_HIGHLIGHT_BYTES
            && self.buffer.lines.iter().all(|line| line.len() <= MAX_HIGHLIGHT_LINE);
        self.syntax = Language::detect(&self.path, self.buffer.line(0))
            .filter(|_| highlightable)
            .map(|language| {
                let mut lines: Vec<LineSyntax> = (0..self.buffer.len()).map(|_| LineSyntax::stale()).collect();
                lines[0].start = Some(language.start());
                Highlighting { language, lines, task: None }
            });
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.doc, Doc::Ready) {
            return;
        }
        let text = self.buffer.text(if self.crlf { "\r\n" } else { "\n" });
        match crate::services::rpc::on(self.server.as_ref(), || write_file(&self.path, &text)) {
            Ok(stamp) => {
                self.disk = Some(stamp);
                self.saved_id = self.state_id();
                self.conflict = None;
                self.error = None;
                self.sync_now();
                if let Some(attached) = &self.lsp {
                    attached.client.notify("textDocument/didSave", json!({ "textDocument": { "uri": self.lsp_uri } }));
                }
            }
            Err(error) => self.error = Some(tf("Couldn't save: {}.", &[&error])),
        }
        cx.notify();
    }

    /// Called every few seconds with the file's stamp on disk.
    fn check_disk(&mut self, stamp: Option<Stamp>, cx: &mut Context<Self>) {
        if !matches!(self.doc, Doc::Ready) {
            if matches!(self.doc, Doc::Failed(_)) && stamp.is_some() && self.disk != stamp {
                // A file that couldn't be read may be readable now.
                self.disk = stamp;
                self.load(cx);
            }
            return;
        }
        if stamp == self.disk {
            return;
        }
        match stamp {
            None => {
                if self.conflict != Some(Conflict::Deleted) {
                    self.conflict = Some(Conflict::Deleted);
                    cx.notify();
                }
            }
            Some(_) if self.dirty() => {
                if self.conflict != Some(Conflict::Changed) {
                    self.conflict = Some(Conflict::Changed);
                    cx.notify();
                }
            }
            Some(_) => self.reload(cx),
        }
    }

    /// Takes in the file as it is on disk, as an edit (so ⌘Z brings back
    /// what was here). Only the part that changed is replaced, which keeps
    /// the caret, scrolling and highlighting where they were.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let loaded = match crate::services::rpc::on(self.server.as_ref(), || read_file(&self.path)) {
            Ok(loaded) => loaded,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let old = self.buffer.text("\n");
        let new = loaded.text;
        let max = old.len().min(new.len());
        let mut prefix = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
        while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
            prefix -= 1;
        }
        let mut suffix = old
            .bytes()
            .rev()
            .zip(new.bytes().rev())
            .take(max - prefix)
            .take_while(|(a, b)| a == b)
            .count();
        while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
            suffix -= 1;
        }
        self.crlf = loaded.crlf;
        self.conflict = None;
        self.error = None;
        if prefix + suffix < old.len() || prefix + suffix < new.len() {
            let range = self.buffer.pos_at(prefix)..self.buffer.pos_at(old.len() - suffix);
            let (anchor, head) = (self.selection.anchor, self.selection.head);
            let scroll = self.scroll_top;
            self.edit(range, &new[prefix..new.len() - suffix], None, EditKind::Other, cx);
            self.selection = Selection {
                anchor: self.buffer.clamp(anchor),
                head: self.buffer.clamp(head),
            };
            self.scroll_top = scroll;
            self.autoscroll = false;
        }
        self.disk = Some(loaded.stamp);
        self.saved_id = self.state_id();
        cx.notify();
    }

    // Editing --------------------------------------------------------------------------

    /// Replaces `range` with `text`, as one undoable step. The selection
    /// becomes `after`, or a caret after the new text.
    fn edit(&mut self, range: Range<Pos>, text: &str, after: Option<Selection>, kind: EditKind, cx: &mut Context<Self>) {
        if !matches!(self.doc, Doc::Ready) {
            return;
        }
        let range = self.buffer.clamp(range.start)..self.buffer.clamp(range.end);
        let old = self.buffer.slice(range.clone());
        if old.is_empty() && text.is_empty() {
            return;
        }
        let before = self.selection;
        let end = self.apply(range.clone(), text);
        let after = after.unwrap_or(Selection::cursor(end));
        self.selection = after;
        self.redo.clear();

        let now = Instant::now();
        let id = self.take_id();
        let saved_id = self.saved_id;
        let merge = kind == EditKind::Typing
            && old.is_empty()
            && !text.contains('\n')
            && self.undo.last().is_some_and(|last| {
                last.kind == EditKind::Typing
                    && last.id != saved_id
                    && end_after(last.start, &last.new) == range.start
                    && now.duration_since(last.at) < TYPING_GROUP
                    // A word and the space after it undo together; the
                    // next word is a new step.
                    && !(last.new.ends_with(char::is_whitespace) && !text.starts_with(char::is_whitespace))
            });
        match self.undo.last_mut() {
            Some(last) if merge => {
                last.new.push_str(text);
                last.after = after;
                last.at = now;
                last.id = id;
            }
            _ => self.undo.push(Edit {
                id,
                start: range.start,
                old,
                new: text.to_string(),
                before,
                after,
                kind,
                at: now,
            }),
        }
        self.goal_column = None;
        self.autoscroll = true;
        if self.completion.is_some() {
            self.filter_completions();
        }
        cx.notify();
        self.schedule_highlight(cx);
        self.schedule_sync(cx);
    }

    /// Changes the buffer and everything that follows its lines.
    fn apply(&mut self, range: Range<Pos>, text: &str) -> Pos {
        // The server hears about this edit as it was made, in positions from
        // before it (when it takes edits one by one).
        if let Some(attached) = &self.lsp
            && attached.client.incremental_sync()
            && let Some(changes) = &mut self.pending_changes
        {
            let (start, end) = (range.start, range.end);
            changes.push(json!({
                "range": {
                    "start": { "line": start.line, "character": byte_to_utf16(self.buffer.line(start.line), start.col) },
                    "end": { "line": end.line, "character": byte_to_utf16(self.buffer.line(end.line), end.col) },
                },
                "text": text,
            }));
        }
        let end = self.buffer.replace(range.clone(), text);
        self.version += 1;
        self.max_columns = None;
        if let Some(h) = &mut self.syntax {
            // The edited lines need highlighting again; the first keeps its
            // start state (lines above didn't change), and its old colors
            // until then.
            let (first, removed_last) = (range.start.line, range.end.line);
            let start = h.lines[first].start.take();
            let spans = std::mem::take(&mut h.lines[first].spans);
            h.lines.splice(first..=removed_last, (first..=end.line).map(|_| LineSyntax::stale()));
            h.lines[first].start = start;
            h.lines[first].spans = spans;
        }
        end
    }

    fn take_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.undo.pop() else {
            return;
        };
        self.apply(edit.start..end_after(edit.start, &edit.new), &edit.old);
        self.selection = edit.before;
        self.redo.push(edit);
        self.after_history(cx);
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.redo.pop() else {
            return;
        };
        self.apply(edit.start..end_after(edit.start, &edit.old), &edit.new);
        self.selection = edit.after;
        self.undo.push(edit);
        self.after_history(cx);
    }

    fn after_history(&mut self, cx: &mut Context<Self>) {
        self.selection = Selection {
            anchor: self.buffer.clamp(self.selection.anchor),
            head: self.buffer.clamp(self.selection.head),
        };
        self.goal_column = None;
        self.autoscroll = true;
        self.completion = None;
        cx.notify();
        self.schedule_highlight(cx);
        self.schedule_sync(cx);
    }

    fn delete_to(&mut self, to: Pos, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            self.edit(self.selection.range(), "", None, EditKind::Other, cx);
            return;
        }
        let head = self.selection.head;
        self.edit(head.min(to)..head.max(to), "", None, EditKind::Other, cx);
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        let head = self.selection.head;
        let line = self.buffer.line(head.line);
        // In leading spaces, back to the previous indent stop.
        if self.selection.is_empty()
            && head.col > 0
            && let IndentStyle::Spaces(n) = self.indent
            && line[..head.col].bytes().all(|b| b == b' ')
        {
            let to = (head.col - 1) / n * n;
            self.delete_to(Pos::new(head.line, to), cx);
            return;
        }
        let to = self.prev_boundary(head);
        self.delete_to(to, cx);
    }

    fn newline(&mut self, cx: &mut Context<Self>) {
        let range = self.selection.range();
        let line = self.buffer.line(range.start.line);
        let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
        let indent = line[..indent_len.min(range.start.col)].to_string();
        let before = line[..range.start.col].trim_end().chars().last();
        let after = self.buffer.line(range.end.line)[range.end.col..].chars().next();
        let language = self.syntax.as_ref().map(|h| h.language.name());
        let opens = matches!(before, Some('{' | '[' | '('))
            || (before == Some(':') && matches!(language, Some("Python" | "YAML")));
        let unit = self.indent.unit();
        let closes = matches!((before, after), (Some('{'), Some('}')) | (Some('['), Some(']')) | (Some('('), Some(')')));
        if closes {
            // Between brackets: the caret on an indented line of its own,
            // the closing bracket below.
            let text = format!("\n{indent}{unit}\n{indent}");
            let caret = Pos::new(range.start.line + 1, indent.len() + unit.len());
            self.edit(range, &text, Some(Selection::cursor(caret)), EditKind::Other, cx);
        } else if opens {
            self.edit(range, &format!("\n{indent}{unit}"), None, EditKind::Other, cx);
        } else {
            self.edit(range, &format!("\n{indent}"), None, EditKind::Other, cx);
        }
    }

    fn newline_below(&mut self, cx: &mut Context<Self>) {
        let line = self.selection.head.line;
        let end = Pos::new(line, self.buffer.line(line).len());
        self.selection = Selection::cursor(end);
        self.newline(cx);
    }

    /// The lines the selection covers (not counting a line it only ends at
    /// the start of).
    fn selected_lines(&self) -> (usize, usize) {
        let range = self.selection.range();
        let last = if range.end.line > range.start.line && range.end.col == 0 {
            range.end.line - 1
        } else {
            range.end.line
        };
        (range.start.line, last)
    }

    /// Changes each line in `first..=last` with `change`, which returns
    /// where in the line it changes: (offset, bytes removed, text
    /// inserted). The selection moves with the text.
    fn change_lines(
        &mut self,
        first: usize,
        last: usize,
        change: impl Fn(&str) -> Option<(usize, usize, String)>,
        cx: &mut Context<Self>,
    ) {
        let changes: Vec<Option<(usize, usize, String)>> =
            self.buffer.lines[first..=last].iter().map(|line| change(line)).collect();
        if changes.iter().all(Option::is_none) {
            return;
        }
        let new_lines: Vec<String> = self.buffer.lines[first..=last]
            .iter()
            .zip(&changes)
            .map(|(line, change)| match change {
                Some((at, removed, inserted)) => format!("{}{inserted}{}", &line[..*at], &line[at + removed..]),
                None => line.clone(),
            })
            .collect();
        let map = |pos: Pos| -> Pos {
            if pos.line < first || pos.line > last {
                return pos;
            }
            match &changes[pos.line - first] {
                None => pos,
                Some((at, removed, inserted)) => Pos::new(
                    pos.line,
                    if pos.col < *at {
                        pos.col
                    } else if pos.col < at + removed {
                        at + inserted.len()
                    } else {
                        pos.col - removed + inserted.len()
                    },
                ),
            }
        };
        let after = Selection {
            anchor: map(self.selection.anchor),
            head: map(self.selection.head),
        };
        let range = Pos::new(first, 0)..Pos::new(last, self.buffer.line(last).len());
        self.edit(range, &new_lines.join("\n"), Some(after), EditKind::Other, cx);
    }

    fn indent(&mut self, cx: &mut Context<Self>) {
        let range = self.selection.range();
        if range.start.line == range.end.line {
            let line = self.buffer.line(range.start.line);
            let text = match self.indent {
                IndentStyle::Tabs => "\t".to_string(),
                IndentStyle::Spaces(n) => " ".repeat(n - columns(line, range.start.col) % n),
            };
            self.edit(range, &text, None, EditKind::Typing, cx);
            return;
        }
        let (first, last) = self.selected_lines();
        let unit = self.indent.unit();
        self.change_lines(first, last, |line| (!line.is_empty()).then(|| (0, 0, unit.clone())), cx);
    }

    fn outdent(&mut self, cx: &mut Context<Self>) {
        let (first, last) = self.selected_lines();
        let width = match self.indent {
            IndentStyle::Tabs => TAB_WIDTH,
            IndentStyle::Spaces(n) => n,
        };
        self.change_lines(
            first,
            last,
            |line| {
                if line.starts_with('\t') {
                    return Some((0, 1, String::new()));
                }
                let spaces = line.len() - line.trim_start_matches(' ').len();
                (spaces > 0).then(|| (0, spaces.min(width), String::new()))
            },
            cx,
        );
    }

    fn toggle_comment(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.syntax.as_ref().and_then(|h| h.language.line_comment()) else {
            return;
        };
        let (first, last) = self.selected_lines();
        let lines = &self.buffer.lines[first..=last];
        let indents: Vec<usize> = lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.len() - line.trim_start().len())
            .collect();
        let Some(&min_indent) = indents.iter().min() else {
            return;
        };
        let commented = lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .all(|line| line.trim_start().starts_with(token));
        self.change_lines(
            first,
            last,
            move |line| {
                if line.trim().is_empty() {
                    return None;
                }
                if commented {
                    let at = line.len() - line.trim_start().len();
                    let space = line[at + token.len()..].starts_with(' ');
                    Some((at, token.len() + usize::from(space), String::new()))
                } else {
                    Some((min_indent, 0, format!("{token} ")))
                }
            },
            cx,
        );
    }

    fn delete_line(&mut self, cx: &mut Context<Self>) {
        let (first, last) = self.selected_lines();
        let len = self.buffer.len();
        let range = if last + 1 < len {
            Pos::new(first, 0)..Pos::new(last + 1, 0)
        } else if first > 0 {
            Pos::new(first - 1, self.buffer.line(first - 1).len())..Pos::new(last, self.buffer.line(last).len())
        } else {
            Pos::new(0, 0)..Pos::new(last, self.buffer.line(last).len())
        };
        let caret = Pos::new(first.min(len.saturating_sub(last - first + 2)), 0);
        self.edit(range, "", Some(Selection::cursor(caret)), EditKind::Other, cx);
        self.selection = Selection::cursor(self.buffer.clamp(caret));
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = if self.selection.is_empty() {
            // No selection: the whole line.
            let line = format!("{}\n", self.buffer.line(self.selection.head.line));
            self.line_clipboard = Some(line.clone());
            line
        } else {
            self.line_clipboard = None;
            self.buffer.slice(self.selection.range())
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        self.copy(cx);
        if self.selection.is_empty() {
            self.delete_line(cx);
        } else {
            self.edit(self.selection.range(), "", None, EditKind::Other, cx);
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let text = text.replace("\r\n", "\n");
        if self.selection.is_empty() && self.line_clipboard.as_deref() == Some(text.as_str()) {
            // A copied line goes in above the caret's line.
            let head = self.selection.head;
            let caret = Pos::new(head.line + text.matches('\n').count(), head.col);
            self.edit(Pos::new(head.line, 0)..Pos::new(head.line, 0), &text, Some(Selection::cursor(caret)), EditKind::Other, cx);
        } else {
            self.edit(self.selection.range(), &text, None, EditKind::Other, cx);
        }
    }

    // Moving ---------------------------------------------------------------------------

    fn move_to(&mut self, to: Pos, select: bool, cx: &mut Context<Self>) {
        let to = self.buffer.clamp(to);
        if select {
            self.selection.head = to;
        } else {
            self.selection = Selection::cursor(to);
        }
        self.goal_column = None;
        self.marked = None;
        self.autoscroll = true;
        self.completion = None;
        cx.notify();
    }

    fn prev_boundary(&self, pos: Pos) -> Pos {
        if pos.col == 0 {
            return if pos.line == 0 {
                pos
            } else {
                Pos::new(pos.line - 1, self.buffer.line(pos.line - 1).len())
            };
        }
        let line = self.buffer.line(pos.line);
        let col = line
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < pos.col).then_some(ix))
            .unwrap_or(0);
        Pos::new(pos.line, col)
    }

    fn next_boundary(&self, pos: Pos) -> Pos {
        let line = self.buffer.line(pos.line);
        if pos.col >= line.len() {
            return if pos.line + 1 < self.buffer.len() { Pos::new(pos.line + 1, 0) } else { pos };
        }
        let col = line
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > pos.col).then_some(ix))
            .unwrap_or(line.len());
        Pos::new(pos.line, col)
    }

    fn word_left(&self, pos: Pos) -> Pos {
        if pos.col == 0 {
            return self.prev_boundary(pos);
        }
        let line = self.buffer.line(pos.line);
        let mut chars = line[..pos.col].char_indices().rev().peekable();
        while chars.peek().is_some_and(|(_, ch)| char_kind(*ch) == CharKind::Space) {
            chars.next();
        }
        let Some(&(_, first)) = chars.peek() else {
            return Pos::new(pos.line, 0);
        };
        let kind = char_kind(first);
        let mut col = pos.col;
        for (ix, ch) in chars {
            if char_kind(ch) != kind {
                break;
            }
            col = ix;
        }
        Pos::new(pos.line, col.min(pos.col))
    }

    fn word_right(&self, pos: Pos) -> Pos {
        let line = self.buffer.line(pos.line);
        if pos.col >= line.len() {
            return self.next_boundary(pos);
        }
        let mut chars = line[pos.col..].char_indices().peekable();
        while chars.peek().is_some_and(|(_, ch)| char_kind(*ch) == CharKind::Space) {
            chars.next();
        }
        let Some(&(_, first)) = chars.peek() else {
            return Pos::new(pos.line, line.len());
        };
        let kind = char_kind(first);
        for (ix, ch) in chars {
            if char_kind(ch) != kind {
                return Pos::new(pos.line, pos.col + ix);
            }
        }
        Pos::new(pos.line, line.len())
    }

    /// The word (or run of spaces or punctuation) around `pos`.
    fn word_at(&self, pos: Pos) -> Range<Pos> {
        let line = self.buffer.line(pos.line);
        let kind = line[pos.col..]
            .chars()
            .next()
            .or_else(|| line[..pos.col].chars().last())
            .map(char_kind);
        let Some(kind) = kind else {
            return pos..pos;
        };
        let start = line[..pos.col]
            .char_indices()
            .rev()
            .take_while(|(_, ch)| char_kind(*ch) == kind)
            .last()
            .map_or(pos.col, |(ix, _)| ix);
        let end = line[pos.col..]
            .char_indices()
            .find(|(_, ch)| char_kind(*ch) != kind)
            .map_or(line.len(), |(ix, _)| pos.col + ix);
        Pos::new(pos.line, start)..Pos::new(pos.line, end)
    }

    /// ⌘← goes to the first non-blank character, then to column 0.
    fn line_start(&self, pos: Pos) -> Pos {
        let line = self.buffer.line(pos.line);
        let text_start = line.len() - line.trim_start().len();
        Pos::new(pos.line, if pos.col == text_start { 0 } else { text_start })
    }

    fn move_vertically(&mut self, rows: isize, select: bool, cx: &mut Context<Self>) {
        let head = self.selection.head;
        if !select && !self.selection.is_empty() && rows.abs() == 1 {
            // An arrow collapses a selection to its edge first.
            let range = self.selection.range();
            let edge = if rows < 0 { range.start } else { range.end };
            self.selection = Selection::cursor(edge);
        }
        let head = if select { head } else { self.selection.head };
        let goal = self.goal_column.unwrap_or_else(|| columns(self.buffer.line(head.line), head.col));
        let target = head.line as isize + rows;
        let to = if target < 0 {
            Pos::new(0, 0)
        } else if target as usize >= self.buffer.len() {
            self.buffer.end()
        } else {
            let line = target as usize;
            Pos::new(line, col_at_columns(self.buffer.line(line), goal))
        };
        self.move_to(to, select, cx);
        self.goal_column = Some(goal);
    }

    fn page(&self) -> isize {
        self.rows.saturating_sub(2).max(1) as isize
    }

    fn horizontal(&mut self, forward: bool, select: bool, cx: &mut Context<Self>) {
        if !select && !self.selection.is_empty() {
            let range = self.selection.range();
            self.move_to(if forward { range.end } else { range.start }, false, cx);
            return;
        }
        let head = self.selection.head;
        let to = if forward { self.next_boundary(head) } else { self.prev_boundary(head) };
        self.move_to(to, select, cx);
    }

    // Mouse and scrolling ---------------------------------------------------------------

    fn pos_for_point(&self, point: Point<Pixels>) -> Pos {
        let Some(layout) = self.layout else {
            return self.selection.head;
        };
        let row = ((point.y - layout.origin.y) / layout.line_height).floor().max(0.) as usize;
        let line_ix = row.min(self.buffer.len() - 1);
        let line = self.buffer.line(line_ix);
        let x = point.x - layout.origin.x;
        let display = match self.shaped.iter().find(|(ix, _)| *ix == line_ix) {
            Some((_, Some(shaped))) => shaped.closest_index_for_x(x),
            Some((_, None)) => 0,
            None => {
                let column = (x / layout.char_width).round().max(0.) as usize;
                return Pos::new(line_ix, col_at_columns(line, column));
            }
        };
        Pos::new(line_ix, from_display(line, display))
    }

    /// The x of `col` in line `line_ix`, from column 0.
    fn x_for(&self, line_ix: usize, col: usize) -> Pixels {
        let line = self.buffer.line(line_ix);
        match self.shaped.iter().find(|(ix, _)| *ix == line_ix) {
            Some((_, Some(shaped))) => shaped.x_for_index(to_display(line, col)),
            Some((_, None)) => px(0.),
            None => self.layout.map_or(px(0.), |l| l.char_width * columns(line, col) as f32),
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.completion = None;
        let pos = self.pos_for_point(event.position);
        // ⌘-click goes to the definition.
        if event.modifiers.secondary() && event.click_count == 1 && self.lsp.is_some() {
            self.selection = Selection::cursor(pos);
            self.go_to_definition(pos, cx);
            cx.notify();
            return;
        }
        match event.click_count {
            2 => {
                let word = self.word_at(pos);
                self.selection = Selection {
                    anchor: word.start,
                    head: word.end,
                };
            }
            3.. => {
                let next = if pos.line + 1 < self.buffer.len() {
                    Pos::new(pos.line + 1, 0)
                } else {
                    Pos::new(pos.line, self.buffer.line(pos.line).len())
                };
                self.selection = Selection {
                    anchor: Pos::new(pos.line, 0),
                    head: next,
                };
            }
            _ if event.modifiers.shift => self.selection.head = pos,
            _ => self.selection = Selection::cursor(pos),
        }
        self.selecting = true;
        self.goal_column = None;
        self.marked = None;
        cx.notify();
    }

    fn drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let pos = self.pos_for_point(position);
        if pos != self.selection.head {
            self.selection.head = pos;
            self.autoscroll = true;
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Over a window you're not in, or to zoom, scrolling goes to the
        // canvas.
        let zooming = event.modifiers.control || event.modifiers.platform;
        let Some(layout) = self.layout else {
            return;
        };
        if zooming || !self.contains_focus(window, cx) {
            return;
        }
        cx.stop_propagation();
        let delta = event.delta.pixel_delta(layout.line_height);
        let zoom = f32::from(layout.line_height) / LINE_HEIGHT;
        self.scroll_top -= delta.y / layout.line_height;
        self.scroll_left -= f32::from(delta.x) / zoom;
        self.clamp_scroll(zoom);
        cx.notify();
    }

    fn widest_line(&mut self) -> usize {
        *self.max_columns.get_or_insert_with(|| {
            self.buffer.lines.iter().map(|line| columns(line, line.len())).max().unwrap_or(0)
        })
    }

    fn clamp_scroll(&mut self, zoom: f32) {
        let max_top = (self.buffer.len() as f32 - 1.).max(0.);
        self.scroll_top = self.scroll_top.clamp(0., max_top);
        // Finding the widest line reads every line: only when scrolled
        // sideways.
        if self.scroll_left > 0. {
            let char_width = self.layout.map_or(8., |l| f32::from(l.char_width) / zoom);
            let max_left = (self.widest_line() as f32 * char_width - 40.).max(0.);
            self.scroll_left = self.scroll_left.clamp(0., max_left);
        } else {
            self.scroll_left = 0.;
        }
    }

    /// Called while painting: scrolls the caret into view if it moved.
    fn update_scroll(&mut self, text: Size<Pixels>, line_height: Pixels, char_width: Pixels) {
        let zoom = f32::from(line_height) / LINE_HEIGHT;
        let rows = (text.height / line_height).max(1.);
        self.rows = rows as usize;
        if std::mem::take(&mut self.autoscroll) {
            let head = self.selection.head;
            let margin = if rows > 10. { 3. } else { 0. };
            let line = head.line as f32;
            if line < self.scroll_top + margin {
                self.scroll_top = line - margin;
            } else if line + 1. > self.scroll_top + rows - margin {
                self.scroll_top = line + 1. + margin - rows;
            }
            let char_width = f32::from(char_width) / zoom;
            let x = columns(self.buffer.line(head.line), head.col) as f32 * char_width;
            let width = f32::from(text.width) / zoom - char_width * 4.;
            if x < self.scroll_left {
                self.scroll_left = (x - char_width * 8.).max(0.);
            } else if x > self.scroll_left + width {
                self.scroll_left = x - width + char_width * 8.;
            }
        }
        self.clamp_scroll(zoom);
    }

    // Highlighting ---------------------------------------------------------------------

    /// Highlights the next stretch of out-of-date lines in the background,
    /// then the one after, until everything's up to date.
    fn schedule_highlight(&mut self, cx: &mut Context<Self>) {
        let len = self.buffer.len();
        let Some(h) = &mut self.syntax else {
            return;
        };
        if h.task.is_some() {
            // It schedules the next one when it's done.
            return;
        }
        let Some(mut first) = h.lines.iter().position(|line| line.stale) else {
            return;
        };
        // Start where the state is known (line 0 always is).
        while h.lines[first].start.is_none() {
            first -= 1;
            h.lines[first].stale = true;
        }
        let state = h.lines[first].start.clone().expect("known start");
        let lines = self.buffer.lines[first..(first + CHUNK).min(len)].to_vec();
        let language = h.language;
        let version = self.version;
        h.task = Some(cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move { language.highlight(state, &lines) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(h) = &mut this.syntax {
                    h.task = None;
                }
                // Edited meanwhile: these lines may have moved. Start over.
                if this.version == version {
                    this.apply_highlights(first, results);
                    cx.notify();
                }
                this.schedule_highlight(cx);
            })
            .ok();
        }));
    }

    fn apply_highlights(&mut self, first: usize, results: Vec<(Vec<Span>, syntax::State)>) {
        let Some(h) = &mut self.syntax else {
            return;
        };
        for (offset, (spans, after)) in results.into_iter().enumerate() {
            let ix = first + offset;
            h.lines[ix].spans = spans;
            h.lines[ix].stale = false;
            let Some(next) = h.lines.get_mut(ix + 1) else {
                break;
            };
            // The next line starts as it did before: from here on, nothing
            // changed.
            if !next.stale && next.start.as_ref() == Some(&after) {
                break;
            }
            next.start = Some(after);
            next.stale = true;
        }
    }

    /// The colored runs for line `ix`, whose display text is `display_len`
    /// bytes long.
    fn runs(&self, ix: usize, line: &str, display_len: usize, base: &Font, theme: &Theme) -> Vec<TextRun> {
        let run = |len: usize, token: syntax::Token| {
            let (color, style, weight) = syntax::style(token, theme);
            TextRun {
                len,
                font: Font {
                    style,
                    weight,
                    ..base.clone()
                },
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }
        };
        let spans: &[Span] = self
            .syntax
            .as_ref()
            .and_then(|h| h.lines.get(ix))
            .map_or(&[], |line| &line.spans);
        let mut runs = Vec::new();
        let mut at = 0;
        for span in spans {
            // Spans from before an edit can be off: keep them in the line,
            // on char boundaries, in order.
            let start = to_display(line, snap(line, span.start as usize));
            let end = to_display(line, snap(line, span.end as usize));
            if start < at || end <= start {
                continue;
            }
            if start > at {
                runs.push(run(start - at, syntax::Token::Plain));
            }
            runs.push(run(end - start, span.token));
            at = end;
        }
        if at < display_len {
            runs.push(run(display_len - at, syntax::Token::Plain));
        }
        runs
    }

    // Rendering ------------------------------------------------------------------------

    fn render_banner(&self, theme: &Theme, cx: &Context<Self>) -> Option<impl IntoElement> {
        let (text, tint, buttons): (String, _, Vec<gpui::AnyElement>) = if let Some(error) = &self.error {
            (error.clone(), theme.tints.destructive, Vec::new())
        } else {
            match self.conflict? {
                Conflict::Changed => (
                    t("This file changed on disk.").to_string(),
                    theme.tints.amber,
                    vec![
                        ui::compact_button("editor-reload", Some(IconName::RefreshCw), t("Reload"), false, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
                            .into_any_element(),
                        ui::compact_button("editor-keep", None, t("Keep mine"), false, theme)
                            .on_click(cx.listener(|this, _, _, cx| {
                                // Saving overwrites the disk's version.
                                this.disk = crate::services::rpc::on(this.server.as_ref(), || stamp_of(&this.path));
                                this.conflict = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                    ],
                ),
                Conflict::Deleted => (
                    t("This file was deleted. Saving creates it again.").to_string(),
                    theme.tints.amber,
                    Vec::new(),
                ),
            }
        };
        Some(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_2()
                .min_h_10()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .bg(tint.base.opacity(0.1))
                .text_size(rems(0.75))
                .child(div().flex_1().min_w_0().text_color(tint.soft_foreground).child(text))
                .children(buttons),
        )
    }

    fn render_status(&self, theme: &Theme) -> impl IntoElement {
        let head = self.selection.head;
        let column = columns(self.buffer.line(head.line), head.col) + 1;
        let selected = if self.selection.is_empty() {
            String::new()
        } else {
            let text = self.buffer.slice(self.selection.range());
            tf(" ({} selected)", &[&text.chars().count()])
        };
        let language = self.syntax.as_ref().map_or(t("Plain Text"), |h| h.language.name());
        let count = |severity: u8| self.diagnostics.iter().filter(|d| d.severity == severity).count();
        let server = self.lsp.as_ref().map(|attached| {
            let (errors, warnings) = (count(1), count(2));
            match (errors, warnings) {
                (0, 0) => attached.client.name.to_string(),
                _ => format!(
                    "{} · {}, {}",
                    attached.client.name,
                    tn(errors, "{} error", "{} errors"),
                    tn(warnings, "{} warning", "{} warnings")
                ),
            }
        });
        let items = [
            format!("{}{selected}", tf("Ln {}, Col {}", &[&(head.line + 1), &column])),
            server.unwrap_or_default(),
            self.indent.label(),
            if self.crlf { "CRLF" } else { "LF" }.to_string(),
            language.to_string(),
        ];
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_4()
            .h_6()
            .px_3()
            .border_t_1()
            .border_color(theme.border)
            .text_size(rems(0.6875))
            .text_color(theme.muted_foreground)
            .child(div().flex_1().min_w_0().truncate().child(items[0].clone()))
            .children(items[1..].iter().filter(|item| !item.is_empty()).map(|item| div().flex_shrink_0().child(item.clone())))
    }

    /// The completion list, under the caret.
    fn render_completions(&self, theme: &Theme, cx: &Context<Self>) -> Option<impl IntoElement> {
        let completion = self.completion.as_ref()?;
        let at = self.caret_point?;
        const VISIBLE: usize = 10;
        let first = completion.selected.saturating_sub(VISIBLE - 1);
        let rows = completion.shown.iter().enumerate().skip(first).take(VISIBLE).map(|(position, &ix)| {
            let item = &completion.items[ix];
            let selected = position == completion.selected;
            let (letter, tint) = completion_badge(item.kind, theme);
            div()
                .id(ElementId::named_usize("completion", position))
                .flex()
                .items_center()
                .gap_2()
                .h(px(24.))
                .px_2()
                .rounded(px(6.))
                .when(selected, |row| row.bg(theme.accent))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(14.))
                        .text_color(tint)
                        .font_weight(FontWeight::BOLD)
                        .child(letter),
                )
                .child(div().flex_shrink_0().max_w(px(260.)).truncate().child(item.label.clone()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(item.detail.clone()),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(completion) = &mut this.completion {
                        completion.selected = position;
                    }
                    this.accept_completion(cx);
                }))
        });
        Some(
            deferred(
                anchored().position(at).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("completions")
                        .occlude()
                        .flex()
                        .flex_col()
                        .w(px(420.))
                        .p_1()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .text_color(theme.popover_foreground)
                        .font_family(FONT_MONO)
                        .text_size(px(12.))
                        .children(rows),
                ),
            )
            .with_priority(2),
        )
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let root = div()
            .id("editor")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground);
        match &self.doc {
            Doc::Loading => root.child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(rems(0.8125))
                    .text_color(theme.muted_foreground)
                    .child(t("Opening…")),
            ),
            Doc::Failed(error) => {
                let path = self.path.clone();
                root.child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .px_8()
                        .text_center()
                        .text_size(rems(0.8125))
                        .child(div().text_color(theme.muted_foreground).child(error.clone()))
                        .when(self.server.is_none() && self.path.exists(), |el| {
                            el.child(
                                ui::compact_button("editor-open-external", Some(IconName::ExternalLink), t("Open with default app"), false, theme)
                                    .on_click(move |_, _, _| open_with_default_app(&path, false)),
                            )
                        }),
                )
            }
            Doc::Ready => root
                .on_action(cx.listener(|this, _: &MoveLeft, _, cx| this.horizontal(false, false, cx)))
                .on_action(cx.listener(|this, _: &MoveRight, _, cx| this.horizontal(true, false, cx)))
                .on_action(cx.listener(|this, _: &SelectLeft, _, cx| this.horizontal(false, true, cx)))
                .on_action(cx.listener(|this, _: &SelectRight, _, cx| this.horizontal(true, true, cx)))
                .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                    if this.completion.is_some() {
                        this.move_completion(-1, cx)
                    } else {
                        this.move_vertically(-1, false, cx)
                    }
                }))
                .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                    if this.completion.is_some() {
                        this.move_completion(1, cx)
                    } else {
                        this.move_vertically(1, false, cx)
                    }
                }))
                .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.move_vertically(-1, true, cx)))
                .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.move_vertically(1, true, cx)))
                .on_action(cx.listener(|this, _: &PageUp, _, cx| this.move_vertically(-this.page(), false, cx)))
                .on_action(cx.listener(|this, _: &PageDown, _, cx| this.move_vertically(this.page(), false, cx)))
                .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.move_vertically(-this.page(), true, cx)))
                .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.move_vertically(this.page(), true, cx)))
                .on_action(cx.listener(|this, _: &MoveWordLeft, _, cx| {
                    this.move_to(this.word_left(this.selection.head), false, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveWordRight, _, cx| {
                    this.move_to(this.word_right(this.selection.head), false, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectWordLeft, _, cx| {
                    this.move_to(this.word_left(this.selection.head), true, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectWordRight, _, cx| {
                    this.move_to(this.word_right(this.selection.head), true, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveLineStart, _, cx| {
                    this.move_to(this.line_start(this.selection.head), false, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectLineStart, _, cx| {
                    this.move_to(this.line_start(this.selection.head), true, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveLineEnd, _, cx| {
                    let line = this.selection.head.line;
                    this.move_to(Pos::new(line, this.buffer.line(line).len()), false, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectLineEnd, _, cx| {
                    let line = this.selection.head.line;
                    this.move_to(Pos::new(line, this.buffer.line(line).len()), true, cx)
                }))
                .on_action(cx.listener(|this, _: &MoveFileStart, _, cx| this.move_to(Pos::default(), false, cx)))
                .on_action(cx.listener(|this, _: &MoveFileEnd, _, cx| this.move_to(this.buffer.end(), false, cx)))
                .on_action(cx.listener(|this, _: &SelectFileStart, _, cx| this.move_to(Pos::default(), true, cx)))
                .on_action(cx.listener(|this, _: &SelectFileEnd, _, cx| this.move_to(this.buffer.end(), true, cx)))
                .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                    this.selection = Selection {
                        anchor: Pos::default(),
                        head: this.buffer.end(),
                    };
                    cx.notify();
                }))
                .on_action(cx.listener(|this, _: &Backspace, _, cx| this.backspace(cx)))
                .on_action(cx.listener(|this, _: &Delete, _, cx| {
                    let to = this.next_boundary(this.selection.head);
                    this.delete_to(to, cx)
                }))
                .on_action(cx.listener(|this, _: &DeleteWordLeft, _, cx| {
                    let to = this.word_left(this.selection.head);
                    this.delete_to(to, cx)
                }))
                .on_action(cx.listener(|this, _: &DeleteWordRight, _, cx| {
                    let to = this.word_right(this.selection.head);
                    this.delete_to(to, cx)
                }))
                .on_action(cx.listener(|this, _: &DeleteToLineStart, _, cx| {
                    let head = this.selection.head;
                    let to = if head.col == 0 { this.prev_boundary(head) } else { Pos::new(head.line, 0) };
                    this.delete_to(to, cx)
                }))
                .on_action(cx.listener(|this, _: &DeleteToLineEnd, _, cx| {
                    let head = this.selection.head;
                    let len = this.buffer.line(head.line).len();
                    let to = if head.col >= len { this.next_boundary(head) } else { Pos::new(head.line, len) };
                    this.delete_to(to, cx)
                }))
                .on_action(cx.listener(|this, _: &DeleteLine, _, cx| this.delete_line(cx)))
                .on_action(cx.listener(|this, _: &Newline, _, cx| {
                    if this.completion.is_some() {
                        this.accept_completion(cx)
                    } else {
                        this.newline(cx)
                    }
                }))
                .on_action(cx.listener(|this, _: &NewlineBelow, _, cx| this.newline_below(cx)))
                .on_action(cx.listener(|this, _: &Indent, _, cx| {
                    if this.completion.is_some() {
                        this.accept_completion(cx)
                    } else {
                        this.indent(cx)
                    }
                }))
                .on_action(cx.listener(|this, _: &Outdent, _, cx| this.outdent(cx)))
                .on_action(cx.listener(|this, _: &ToggleComment, _, cx| this.toggle_comment(cx)))
                .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
                .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut(cx)))
                .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
                .on_action(cx.listener(|this, _: &Undo, _, cx| this.undo(cx)))
                .on_action(cx.listener(|this, _: &Redo, _, cx| this.redo(cx)))
                .on_action(cx.listener(|this, _: &Save, _, cx| this.save(cx)))
                .on_action(cx.listener(|this, _: &Escape, _, cx| {
                    if this.completion.take().is_some() {
                        cx.notify();
                    } else if this.selection.is_empty() {
                        cx.propagate();
                    } else {
                        this.selection = Selection::cursor(this.selection.head);
                        cx.notify();
                    }
                }))
                .on_action(cx.listener(|_, _: &ShowCharacterPalette, window, _| window.show_character_palette()))
                .on_action(cx.listener(|this, _: &GoToDefinition, _, cx| {
                    let head = this.selection.head;
                    this.go_to_definition(head, cx)
                }))
                .on_action(cx.listener(|this, _: &ShowCompletions, _, cx| this.request_completions(cx)))
                .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                .children(self.render_banner(theme, cx))
                .child(
                    div()
                        .id("editor-text")
                        .flex_1()
                        .min_h_0()
                        .cursor(CursorStyle::IBeam)
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                        .child(EditorElement { editor: cx.entity() }),
                )
                .child(self.render_status(theme))
                .children(self.render_completions(theme, cx)),
        }
    }
}

impl EventEmitter<EditorEvent> for EditorView {}

/// Typed text and IME composition. The IME sees the caret's line as the
/// whole document: offsets are UTF-16 within it.
impl EntityInputHandler for EditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let line = self.buffer.line(self.selection.head.line);
        let range = utf16_range(line, &range_utf16);
        actual_range.replace(byte_to_utf16(line, range.start)..byte_to_utf16(line, range.end));
        Some(line[range].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let head = self.selection.head;
        let line = self.buffer.line(head.line);
        let range = self.selection.range();
        let range = if range.start.line == head.line && range.end.line == head.line {
            byte_to_utf16(line, range.start.col)..byte_to_utf16(line, range.end.col)
        } else {
            let caret = byte_to_utf16(line, head.col);
            caret..caret
        };
        Some(UTF16Selection {
            range,
            reversed: self.selection.head < self.selection.anchor,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let line = self.buffer.line(self.selection.head.line);
        self.marked
            .as_ref()
            .map(|range| byte_to_utf16(line, range.start)..byte_to_utf16(line, range.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let line_ix = self.selection.head.line;
        let line = self.buffer.line(line_ix);
        let range = match (range_utf16, &self.marked) {
            (Some(range), _) => {
                let range = utf16_range(line, &range);
                Pos::new(line_ix, range.start)..Pos::new(line_ix, range.end)
            }
            (None, Some(marked)) => Pos::new(line_ix, marked.start)..Pos::new(line_ix, marked.end),
            (None, None) => self.selection.range(),
        };
        self.marked = None;
        let kind = if text.contains('\n') { EditKind::Other } else { EditKind::Typing };
        self.edit(range, &text, None, kind, cx);
        self.after_typing(&text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_ix = self.selection.head.line;
        let line = self.buffer.line(line_ix);
        let range = match (range_utf16, &self.marked) {
            (Some(range), _) => {
                let range = utf16_range(line, &range);
                Pos::new(line_ix, range.start)..Pos::new(line_ix, range.end)
            }
            (None, Some(marked)) => Pos::new(line_ix, marked.start)..Pos::new(line_ix, marked.end),
            (None, None) => self.selection.range(),
        };
        let text = text.replace('\n', " ");
        let start = range.start;
        self.edit(range, &text, None, EditKind::Typing, cx);
        if start.line == self.selection.head.line && !text.is_empty() {
            self.marked = Some(start.col..start.col + text.len());
            if let Some(selected) = selected_utf16 {
                let col = start.col + utf16_to_byte(&text, selected.end);
                self.selection = Selection::cursor(Pos::new(start.line, col));
            }
        } else {
            self.marked = None;
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout?;
        let line_ix = self.selection.head.line;
        let line = self.buffer.line(line_ix);
        let start = self.x_for(line_ix, utf16_to_byte(line, range_utf16.start));
        let end = self.x_for(line_ix, utf16_to_byte(line, range_utf16.end));
        let top = layout.origin.y + layout.line_height * line_ix as f32;
        Some(Bounds::from_corners(
            point(layout.origin.x + start, top),
            point(layout.origin.x + end, top + layout.line_height),
        ))
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let pos = self.pos_for_point(point);
        (pos.line == self.selection.head.line).then(|| byte_to_utf16(self.buffer.line(pos.line), pos.col))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_replace_across_lines() {
        let mut buffer = Buffer::new("one\ntwo\nthree");
        let end = buffer.replace(Pos::new(0, 1)..Pos::new(2, 2), "X\nY");
        assert_eq!(buffer.lines, ["oX", "Yree"]);
        assert_eq!(end, Pos::new(1, 1));
        assert_eq!(buffer.slice(Pos::new(0, 1)..Pos::new(1, 2)), "X\nYr");
        let end = buffer.replace(Pos::new(1, 0)..Pos::new(1, 0), "");
        assert_eq!(end, Pos::new(1, 0));
        assert_eq!(end_after(Pos::new(3, 2), "ab\ncde"), Pos::new(4, 3));
        assert_eq!(end_after(Pos::new(3, 2), "ab"), Pos::new(3, 4));
        assert_eq!(buffer.pos_at(3), Pos::new(1, 0));
        assert_eq!(buffer.pos_at(5), Pos::new(1, 2));
        assert_eq!(buffer.text("\r\n"), "oX\r\nYree");
    }

    #[test]
    fn tabs_map_to_display_columns() {
        let line = "\tif x {";
        assert_eq!(display_text(line), "    if x {");
        assert_eq!(to_display(line, 1), 4);
        assert_eq!(from_display(line, 4), 1);
        assert_eq!(from_display(line, 1), 0, "inside a tab snaps to the nearer side");
        assert_eq!(from_display(line, 3), 1);
        assert_eq!(columns(line, 3), 6);
        assert_eq!(col_at_columns(line, 6), 3);
        assert_eq!(col_at_columns("ab", 9), 2);
        assert_eq!(utf16_to_byte("añb", 2), 3);
        assert_eq!(byte_to_utf16("añb", 3), 2);
    }

    #[test]
    fn indentation_is_detected() {
        let lines = |text: &str| text.lines().map(String::from).collect::<Vec<_>>();
        assert_eq!(IndentStyle::detect(&lines("fn a() {\n\tb();\n\tc();\n}")), IndentStyle::Tabs);
        assert_eq!(IndentStyle::detect(&lines("a:\n  b: 1\n  c:\n    d: 2")), IndentStyle::Spaces(2));
        assert_eq!(IndentStyle::detect(&lines("fn a() {\n    b();\n}")), IndentStyle::Spaces(4));
    }
}
