//! A terminal window: your login shell on a PTY, emulated by
//! `alacritty_terminal` (the engine Zed's terminal uses) and drawn with GPUI.
//!
//! - [`Session`] owns the PTY and the emulator. alacritty's I/O thread reads
//!   the shell's output into the grid and reports events over a channel.
//! - [`TerminalView`] is the GPUI view: focus, keyboard and IME input,
//!   scrolling, paste. Every key goes to the shell, as in any terminal.
//! - [`TerminalElement`] paints the grid: background runs, text runs shaped
//!   on a fixed cell grid, and the cursor. It sizes itself from the rem size
//!   around it, so a zoomed canvas scales the terminal without reflowing it.
//!
//! Not yet: mouse selection and copy, mouse reporting (e.g. clicking in vim),
//! and hyperlinks.

use std::{
    borrow::Cow,
    net::Shutdown,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    sync::FairMutex,
    term::{self, Term, TermMode, cell::Flags, color::Colors},
    vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb},
};

use crate::windows::explorer::DraggedFiles;
use crate::services::server::{self, Hello, HostAddress, Stream, frame};
use futures::{StreamExt, channel::mpsc};
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, ExternalPaths, FocusHandle, Focusable, Font, FontStyle, FontWeight,
    GlobalElementId, Hsla, KeyBinding, KeyDownEvent, Keystroke, LayoutId, Pixels, Point,
    ScrollWheelEvent, ShapedLine, SharedString, Size, Style, StrikethroughStyle, Task, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, font, outline, point, prelude::*,
    px, relative, rems, size,
};

use crate::ui::theme::{FONT_MONO, Mode, Theme};

actions!(terminal, [Paste]);

const KEY_CONTEXT: &str = "Terminal";
/// At 100% zoom; the canvas scales these.
const FONT_SIZE: f32 = 13.;
/// 1.3 × the font size, rounded so rows land on whole pixels.
const LINE_HEIGHT: f32 = 17.;
const SCROLLBACK_LINES: usize = 10_000;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("secondary-v", Paste, Some(KEY_CONTEXT))]);
}

// Session --------------------------------------------------------------------

/// What the reader thread tells the view.
enum SessionEvent {
    /// The screen changed.
    Output,
    /// Something the emulator raised (title, clipboard, replies to the shell).
    Term(TermEvent),
    /// The shell exited.
    Exited,
    /// Another window attached to this session.
    TakenOver,
    /// The host went away (it was killed, or crashed).
    Disconnected,
}

/// Forwards the emulator's events to the view. While replaying history it
/// drops the replies a terminal sends back to the shell (cursor reports,
/// color answers and the like): those questions were asked and answered
/// long ago, and re-answering them would type garbage into the shell.
#[derive(Clone)]
struct Listener {
    events: mpsc::UnboundedSender<SessionEvent>,
    replaying: Arc<AtomicBool>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let reply = matches!(
            event,
            TermEvent::PtyWrite(_)
                | TermEvent::ColorRequest(..)
                | TermEvent::TextAreaSizeRequest(_)
                | TermEvent::ClipboardLoad(..)
                | TermEvent::ClipboardStore(..)
                | TermEvent::Bell
        );
        if reply && self.replaying.load(Ordering::SeqCst) {
            return;
        }
        // The view may already be gone; then nobody needs the event.
        self.events.unbounded_send(SessionEvent::Term(event)).ok();
    }
}

/// The terminal's size in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridSize {
    columns: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// This window's view of a session in the session host: the emulator (fed
/// with the host's output by a reader thread) and the connection.
///
/// Dropping it detaches: the shell keeps running in the host. [`kill`]
/// ends it.
///
/// [`kill`]: Session::kill
struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    connection: Mutex<Stream>,
    size: GridSize,
    /// What the shell is doing, from its prompt marks (see [`ShellState`]).
    shell: Arc<AtomicU8>,
}

/// What the shell is doing, from the prompt marks our startup files print
/// (OSC 133, the convention iTerm, VS Code and Ghostty use).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellState {
    /// No mark seen yet.
    Starting = 0,
    /// At a prompt.
    Prompt = 1,
    /// Running a command.
    Running = 2,
}

impl ShellState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => ShellState::Prompt,
            2 => ShellState::Running,
            _ => ShellState::Starting,
        }
    }
}

/// Finds OSC 133 prompt marks (`ESC ] 133 ; <A|B|C|D>`) in output, including
/// ones split across reads. alacritty ignores these, so we look ourselves.
#[derive(Default)]
struct MarkScanner {
    /// The end of the previous chunk, in case a mark straddles chunks.
    carry: Vec<u8>,
}

impl MarkScanner {
    const MARK: &'static [u8] = b"\x1b]133;";

    /// The shell state the last mark in `bytes` means, if there is one.
    fn scan(&mut self, bytes: &[u8]) -> Option<ShellState> {
        let carried = self.carry.len();
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let mut state = None;
        let width = Self::MARK.len() + 1;
        for start in 0..data.len().saturating_sub(width - 1) {
            // Marks that ended inside the carry were counted last time.
            if start + width <= carried || &data[start..start + Self::MARK.len()] != Self::MARK {
                continue;
            }
            state = match data[start + Self::MARK.len()] {
                b'C' => Some(ShellState::Running),
                b'A' | b'B' | b'D' => Some(ShellState::Prompt),
                _ => state,
            };
        }
        self.carry = data[data.len().saturating_sub(width - 1)..].to_vec();
        state
    }
}

impl Session {
    /// Attaches to session `id`, which the host starts in `folder` if it
    /// isn't running.
    fn attach(
        id: &str,
        folder: Option<PathBuf>,
        address: &HostAddress,
    ) -> Result<(Self, mpsc::UnboundedReceiver<SessionEvent>), String> {
        let (events, receiver) = mpsc::unbounded();
        let replaying = Arc::new(AtomicBool::new(true));
        let listener = Listener {
            events: events.clone(),
            replaying: replaying.clone(),
        };
        // A placeholder size; the first layout resizes it to fit.
        let size = GridSize {
            columns: 80,
            lines: 24,
        };
        let config = term::Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Default::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener)));

        let hello = Hello {
            protocol: server::PROTOCOL,
            session: id.to_string(),
            folder,
            columns: size.columns as u16,
            lines: size.lines as u16,
            attach: true,
        };
        // This machine's server is only started if you haven't stopped it.
        let address = match address {
            HostAddress::Local { can_start: true, .. } => crate::services::rpc::local_address(),
            other => other.clone(),
        };
        let (stream, _) = server::attach(&address, &hello)
            .map_err(|error| format!("Couldn't reach the server: {error}"))?;
        let reader = stream
            .try_clone()
            .map_err(|error| format!("Couldn't read from the terminal: {error}"))?;
        let shell = Arc::new(AtomicU8::new(ShellState::Starting as u8));
        {
            let (term, shell) = (term.clone(), shell.clone());
            std::thread::Builder::new()
                .name("terminal reader".into())
                .spawn(move || read_output(reader, term, shell, replaying, events))
                .map_err(|error| format!("Couldn't start the terminal: {error}"))?;
        }

        Ok((
            Self {
                term,
                connection: Mutex::new(stream),
                size,
                shell,
            },
            receiver,
        ))
    }

    fn shell_state(&self) -> ShellState {
        ShellState::from_u8(self.shell.load(Ordering::SeqCst))
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        let mut connection = self.connection.lock().unwrap();
        server::write_frame(&mut *connection, frame::INPUT, &bytes).ok();
    }

    fn resize(&mut self, size: GridSize, cell: Size<Pixels>) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.term.lock().resize(size);
        let window = window_size(size, cell);
        let mut connection = self.connection.lock().unwrap();
        server::write_frame(&mut *connection, frame::RESIZE, &server::encode_size(window)).ok();
    }

    /// Ends the shell (closing the window, not quitting the app).
    fn kill(&self) {
        let mut connection = self.connection.lock().unwrap();
        server::write_frame(&mut *connection, frame::KILL, &[]).ok();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Detach. Shutting the socket down also wakes the reader thread.
        if let Ok(connection) = self.connection.lock() {
            connection.shutdown(Shutdown::Both).ok();
        }
    }
}

/// Feeds the host's output into the emulator until the session ends.
fn read_output(
    mut reader: Stream,
    term: Arc<FairMutex<Term<Listener>>>,
    shell: Arc<AtomicU8>,
    replaying: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<SessionEvent>,
) {
    let mut parser: Processor = Processor::new();
    let mut marks = MarkScanner::default();
    let ending = loop {
        match server::read_frame(&mut reader) {
            Ok((frame::OUTPUT, bytes)) => {
                if let Some(state) = marks.scan(&bytes) {
                    shell.store(state as u8, Ordering::SeqCst);
                }
                parser.advance(&mut *term.lock(), &bytes);
                events.unbounded_send(SessionEvent::Output).ok();
            }
            Ok((frame::REPLAY_END, _)) => replaying.store(false, Ordering::SeqCst),
            Ok((frame::EXITED, _)) => break SessionEvent::Exited,
            Ok((frame::TAKEN_OVER, _)) => break SessionEvent::TakenOver,
            Ok(_) => {}
            Err(_) => break SessionEvent::Disconnected,
        }
    };
    events.unbounded_send(ending).ok();
}

fn window_size(size: GridSize, cell: Size<Pixels>) -> WindowSize {
    WindowSize {
        num_lines: size.lines as u16,
        num_cols: size.columns as u16,
        cell_width: f32::from(cell.width) as u16,
        cell_height: f32::from(cell.height) as u16,
    }
}

/// Menlo 13px is ~7.8px wide; only used before the first layout.
fn cell_size_guess() -> Size<Pixels> {
    size(px(8.), px(LINE_HEIGHT))
}

// View -------------------------------------------------------------------------

pub enum TerminalEvent {
    /// The shell set the window title (or reset it). At a prompt the title
    /// is the shell's folder; see [`TerminalView::cwd`].
    TitleChanged,
    /// The shell exited.
    Exited,
}

/// What a terminal window needs to find (or start) its shell.
#[derive(Clone)]
pub struct SessionSpec {
    /// The session's id in the host, saved with the window.
    pub id: String,
    /// Where a new shell starts, if the session isn't running any more.
    pub folder: Option<PathBuf>,
    pub host: HostAddress,
}

pub struct TerminalView {
    spec: SessionSpec,
    session: Result<Session, SharedString>,
    /// When we last reattached after losing the host, so a host that keeps
    /// dying doesn't turn into a loop.
    reconnected_at: Option<Instant>,
    /// Title set by the shell (OSC 0/2): its folder, or the running command.
    title: Option<SharedString>,
    focus_handle: FocusHandle,
    exited: bool,
    /// Fractional lines of scroll not applied yet (trackpads scroll in pixels).
    scroll_lines: f32,
    /// IME composition in progress, if any.
    marked_text: Option<String>,
    /// Where the cursor was last painted, for placing the IME window.
    cursor_bounds: Option<Bounds<Pixels>>,
    _events: Option<Task<()>>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    /// Attaches to the window's session, which keeps running in the session
    /// host when the app quits. If it isn't running (first open, or after a
    /// reboot), the host starts a shell in `spec.folder`.
    pub fn new(spec: SessionSpec, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            spec,
            session: Err(SharedString::default()),
            reconnected_at: None,
            title: None,
            focus_handle: cx.focus_handle(),
            exited: false,
            scroll_lines: 0.,
            marked_text: None,
            cursor_bounds: None,
            _events: None,
        };
        this.attach(cx);
        this
    }

    fn attach(&mut self, cx: &mut Context<Self>) {
        let (session, events) = match Session::attach(&self.spec.id, self.spec.folder.clone(), &self.spec.host) {
            Ok((session, events)) => (Ok(session), Some(events)),
            Err(error) => (Err(error.into()), None),
        };
        self.session = session;
        self._events = events.map(|mut events| {
            cx.spawn(async move |this, cx| {
                while let Some(event) = events.next().await {
                    // Output arrives in bursts; handle everything queued, so
                    // a burst is one redraw.
                    let mut batch = vec![event];
                    while let Ok(event) = events.try_recv() {
                        batch.push(event);
                    }
                    let alive = this.update(cx, |this, cx| {
                        for event in batch {
                            this.handle_session_event(event, cx);
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
        });
        cx.notify();
    }

    fn handle_session_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::Output => cx.notify(),
            SessionEvent::Term(event) => self.handle_event(event, cx),
            SessionEvent::Exited => {
                if !self.exited {
                    self.exited = true;
                    cx.emit(TerminalEvent::Exited);
                }
            }
            SessionEvent::TakenOver => {
                self.session = Err("This terminal is open in another xharness window.".into());
                cx.notify();
            }
            SessionEvent::Disconnected => {
                // The host is gone (killed, or crashed). Attach again: that
                // starts a new host, and a fresh shell in the last folder.
                let recently = self
                    .reconnected_at
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(5));
                if recently {
                    self.session = Err("Lost the terminal session host.".into());
                    cx.notify();
                } else {
                    self.reconnected_at = Some(Instant::now());
                    self.attach(cx);
                }
            }
        }
    }

    /// Ends the shell for good (the window is closing). Without this,
    /// dropping the view only detaches and the shell keeps running.
    pub fn kill(&self) {
        if let Ok(session) = &self.session {
            session.kill();
        }
    }

    pub fn title(&self) -> Option<&SharedString> {
        self.title.as_ref()
    }

    /// The shell's folder, when it's at a prompt: our startup files put the
    /// folder's full path in the title there (a command, while one runs).
    pub fn cwd(&self) -> Option<PathBuf> {
        let title = self.title.as_ref()?;
        let path = PathBuf::from(title.as_ref());
        (path.is_absolute() && path.is_dir()).then_some(path)
    }

    /// Gives this terminal the keyboard.
    pub fn focus(&self, window: &mut Window, _: &App) {
        window.focus(&self.focus_handle);
    }

    pub fn contains_focus(&self, window: &Window, _: &App) -> bool {
        self.focus_handle.is_focused(window)
    }

    fn shell_state(&self) -> ShellState {
        match &self.session {
            Ok(session) => session.shell_state(),
            Err(_) => ShellState::Starting,
        }
    }

    /// Whether a command is running (from the shell's prompt marks).
    pub fn running(&self) -> bool {
        self.shell_state() == ShellState::Running
    }

    fn handle_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
            TermEvent::Wakeup => cx.notify(),
            // Replies the terminal owes the shell (cursor position reports
            // and the like).
            TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
            TermEvent::Title(title) => {
                self.title = Some(title.into());
                cx.emit(TerminalEvent::TitleChanged);
                cx.notify();
            }
            TermEvent::ResetTitle => {
                self.title = None;
                cx.emit(TerminalEvent::TitleChanged);
                cx.notify();
            }
            TermEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            TermEvent::ClipboardLoad(_, format) => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                self.write(format(&text).into_bytes());
            }
            TermEvent::ColorRequest(index, format) => {
                let color = color_for_index(index, Theme::global(cx));
                self.write(format(hsla_to_rgb(color)).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                if let Ok(session) = &self.session {
                    self.write(format(window_size(session.size, cell_size_guess())).into_bytes());
                }
            }
            TermEvent::Exit | TermEvent::ChildExit(_) => {
                if !self.exited {
                    self.exited = true;
                    cx.emit(TerminalEvent::Exited);
                }
            }
            TermEvent::Bell | TermEvent::CursorBlinkingChange | TermEvent::MouseCursorDirty => {}
        }
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        if let Ok(session) = &self.session {
            session.write(bytes);
        }
    }

    fn mode(&self) -> TermMode {
        match &self.session {
            Ok(session) => *session.term.lock().mode(),
            Err(_) => TermMode::empty(),
        }
    }

    /// Sends user input, jumping back to the live screen first.
    fn input(&mut self, bytes: impl Into<Cow<'static, [u8]>>, cx: &mut Context<Self>) {
        if let Ok(session) = &self.session {
            session.term.lock().scroll_display(Scroll::Bottom);
            session.write(bytes);
            cx.notify();
        }
    }

    fn resize(&mut self, size: GridSize, cell: Size<Pixels>) {
        if let Ok(session) = &mut self.session {
            session.resize(size, cell);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Text keys are left to the input handler (so IME and dead keys
        // work); only keys that send control sequences are handled here.
        if let Some(bytes) = key_sequence(&event.keystroke, self.mode()) {
            self.input(bytes, cx);
            cx.stop_propagation();
        }
    }

    fn type_paths(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        let mut text = paths
            .iter()
            .map(|path| shell_quote(&path.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        text.push(' ');
        self.input(text.into_bytes(), cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let text = if self.mode().contains(TermMode::BRACKETED_PASTE) {
            // Strip any bracket sequences in the text so it can't end the
            // paste early.
            let text = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.input(text.into_bytes(), cx);
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(LINE_HEIGHT));
        // Only the terminal you're working in scrolls its history. Over any
        // other window, and for zooming (⌘/ctrl) or sideways swipes, the
        // event goes on to the canvas.
        let zooming = event.modifiers.control || event.modifiers.platform;
        let sideways = f32::from(delta.x).abs() > f32::from(delta.y).abs();
        if zooming || sideways || !self.contains_focus(window, cx) {
            return;
        }
        cx.stop_propagation();
        self.scroll_lines += f32::from(delta.y) / LINE_HEIGHT;
        let lines = self.scroll_lines.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_lines -= lines as f32;

        let mode = self.mode();
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // Full-screen apps (less, vim) have no scrollback; like other
            // terminals, scrolling sends arrow keys instead.
            let key: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.write(key.repeat(lines.unsigned_abs() as usize));
        } else if let Ok(session) = &self.session {
            session.term.lock().scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);

        div()
            .id("terminal")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .px_2()
            .py_1()
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::paste))
            .on_key_down(cx.listener(Self::key_down))
            // Files dropped on a terminal type their paths, as in Terminal.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.type_paths(paths.paths(), cx)))
            .on_drop(cx.listener(|this, files: &DraggedFiles, _, cx| this.type_paths(&files.0, cx)))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .map(|terminal| match &self.session {
                Ok(_) => terminal.child(TerminalElement { view: cx.entity() }),
                Err(error) => terminal
                    .p_2()
                    .text_size(rems(FONT_SIZE / 16.))
                    .text_color(theme.tints.destructive.soft_foreground)
                    .child(error.clone()),
            })
    }
}

/// `text` as one shell word: as is when it's only safe characters, else in
/// single quotes.
fn shell_quote(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_-./~+@%:,=".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Typed text arrives here (from the keyboard or an IME) and goes straight
/// to the shell. Nothing is editable, so ranges are always empty.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<std::ops::Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_text = None;
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        if !text.is_empty() {
            self.input(text.to_string().into_bytes(), cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: Option<std::ops::Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!text.is_empty()).then(|| text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// The bytes a terminal sends for a key, for keys that don't just type text.
/// `None` lets the key through as text (or to app shortcuts, for ⌘ keys).
fn key_sequence(keystroke: &Keystroke, mode: TermMode) -> Option<Cow<'static, [u8]>> {
    let mods = &keystroke.modifiers;
    let key = keystroke.key.as_str();

    // macOS text-editing conventions for ⌘, like Terminal.app.
    if mods.platform {
        let bytes: &'static [u8] = match key {
            "left" => b"\x01",      // start of line (ctrl-a)
            "right" => b"\x05",     // end of line (ctrl-e)
            "backspace" => b"\x15", // delete to start of line (ctrl-u)
            _ => return None,
        };
        return Some(bytes.into());
    }

    // xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let modifier = 1 + mods.shift as u8 + 2 * mods.alt as u8 + 4 * mods.control as u8;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor_key = |end: char| -> Cow<'static, [u8]> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{end}").into_bytes().into()
        } else if app_cursor {
            format!("\x1bO{end}").into_bytes().into()
        } else {
            format!("\x1b[{end}").into_bytes().into()
        }
    };
    let tilde_key = |code: u8| -> Cow<'static, [u8]> {
        if modifier > 1 {
            format!("\x1b[{code};{modifier}~").into_bytes().into()
        } else {
            format!("\x1b[{code}~").into_bytes().into()
        }
    };
    let function_key = |end: char| -> Cow<'static, [u8]> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{end}").into_bytes().into()
        } else {
            format!("\x1bO{end}").into_bytes().into()
        }
    };
    let fixed = |bytes: &'static [u8]| -> Option<Cow<'static, [u8]>> { Some(bytes.into()) };

    match key {
        "enter" if mods.alt => fixed(b"\x1b\r"),
        "enter" => fixed(b"\r"),
        "backspace" if mods.control => fixed(b"\x08"),
        "backspace" if mods.alt => fixed(b"\x1b\x7f"),
        "backspace" => fixed(b"\x7f"),
        "tab" if mods.shift => fixed(b"\x1b[Z"),
        "tab" => fixed(b"\t"),
        "escape" => fixed(b"\x1b"),
        // ⌥← / ⌥→ jump words, as in macOS terminals.
        "left" if mods.alt && !mods.control && !mods.shift => fixed(b"\x1bb"),
        "right" if mods.alt && !mods.control && !mods.shift => fixed(b"\x1bf"),
        "up" => Some(cursor_key('A')),
        "down" => Some(cursor_key('B')),
        "right" => Some(cursor_key('C')),
        "left" => Some(cursor_key('D')),
        "home" => Some(cursor_key('H')),
        "end" => Some(cursor_key('F')),
        "insert" => Some(tilde_key(2)),
        "delete" => Some(tilde_key(3)),
        "pageup" => Some(tilde_key(5)),
        "pagedown" => Some(tilde_key(6)),
        "f1" => Some(function_key('P')),
        "f2" => Some(function_key('Q')),
        "f3" => Some(function_key('R')),
        "f4" => Some(function_key('S')),
        "f5" => Some(tilde_key(15)),
        "f6" => Some(tilde_key(17)),
        "f7" => Some(tilde_key(18)),
        "f8" => Some(tilde_key(19)),
        "f9" => Some(tilde_key(20)),
        "f10" => Some(tilde_key(21)),
        "f11" => Some(tilde_key(23)),
        "f12" => Some(tilde_key(24)),
        _ if mods.control => {
            let byte = control_byte(key)?;
            Some(if mods.alt {
                vec![0x1b, byte].into()
            } else {
                vec![byte].into()
            })
        }
        // Everything else (including ⌥ combinations, which type characters
        // on macOS) is text.
        _ => None,
    }
}

/// ctrl + key → the C0 control code, as on a VT100.
fn control_byte(key: &str) -> Option<u8> {
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return (key == "space").then_some(0);
    };
    Some(match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' | ' ' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '/' | '7' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

// Colors -----------------------------------------------------------------------

/// The 16 ANSI colors, built from the Astralyx palette: accents from the
/// theme's tints (bright = their hover shade), neutrals from its grays.
fn ansi_color(index: usize, theme: &Theme) -> Hsla {
    let tints = &theme.tints;
    let dark = theme.mode == Mode::Dark;
    let gray = |hex: u32| -> Hsla { gpui::rgb(hex).into() };
    match index {
        0 => gray(if dark { 0x1d1d1d } else { 0x171717 }),
        1 => tints.destructive.base,
        2 => tints.green.base,
        3 => tints.amber.base,
        4 => tints.blue.base,
        5 => tints.violet.base,
        6 => tints.cyan.base,
        7 => gray(if dark { 0xe5e5e5 } else { 0x868686 }),
        8 => theme.muted_foreground,
        9 => tints.destructive.hover,
        10 => tints.green.hover,
        11 => tints.amber.hover,
        12 => tints.blue.hover,
        13 => tints.violet.hover,
        14 => tints.cyan.hover,
        _ => gray(if dark { 0xffffff } else { 0x333333 }),
    }
}

/// Any palette index: ANSI 0–15, then xterm's 6×6×6 cube and gray ramp, then
/// alacritty's named colors (foreground, background, cursor, dim variants).
fn color_for_index(index: usize, theme: &Theme) -> Hsla {
    const FOREGROUND: usize = NamedColor::Foreground as usize;
    const BACKGROUND: usize = NamedColor::Background as usize;
    const CURSOR: usize = NamedColor::Cursor as usize;
    const DIM_BLACK: usize = NamedColor::DimBlack as usize;
    const DIM_WHITE: usize = NamedColor::DimWhite as usize;
    const BRIGHT_FOREGROUND: usize = NamedColor::BrightForeground as usize;
    const DIM_FOREGROUND: usize = NamedColor::DimForeground as usize;

    match index {
        0..16 => ansi_color(index, theme),
        16..232 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { 55 + v as u32 * 40 };
            gpui::rgb(level(i / 36) << 16 | level(i / 6 % 6) << 8 | level(i % 6)).into()
        }
        232..256 => {
            let v = 8 + (index as u32 - 232) * 10;
            gpui::rgb(v << 16 | v << 8 | v).into()
        }
        FOREGROUND | CURSOR | BRIGHT_FOREGROUND => theme.foreground,
        BACKGROUND => theme.background,
        DIM_BLACK..=DIM_WHITE => ansi_color(index - DIM_BLACK, theme).opacity(0.66),
        DIM_FOREGROUND => theme.foreground.opacity(0.66),
        _ => theme.foreground,
    }
}

fn resolve_color(color: AnsiColor, colors: &Colors, theme: &Theme) -> Hsla {
    let index = match color {
        AnsiColor::Spec(rgb) => return rgb_to_hsla(rgb),
        AnsiColor::Indexed(index) => index as usize,
        AnsiColor::Named(name) => name as usize,
    };
    // Colors the shell redefined (OSC 4/10/11) win over the theme.
    match colors[index] {
        Some(rgb) => rgb_to_hsla(rgb),
        None => color_for_index(index, theme),
    }
}

fn rgb_to_hsla(rgb: Rgb) -> Hsla {
    gpui::rgb((rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32).into()
}

fn hsla_to_rgb(color: Hsla) -> Rgb {
    let rgba = color.to_rgb();
    let channel = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    Rgb {
        r: channel(rgba.r),
        g: channel(rgba.g),
        b: channel(rgba.b),
    }
}

// Rendering --------------------------------------------------------------------

/// Text attributes that split a line into separately shaped runs.
#[derive(Clone, Copy, PartialEq)]
struct CellStyle {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    undercurl: bool,
    strikethrough: bool,
}

/// A run of same-styled cells on one row.
struct Batch {
    row: usize,
    column: usize,
    cells: usize,
    text: String,
    style: CellStyle,
}

struct PaintCursor {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    color: Hsla,
    /// The character under a block cursor, redrawn in the background color.
    glyph: Option<ShapedLine>,
}

struct TerminalPrepaint {
    line_height: Pixels,
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    cursor: Option<PaintCursor>,
}

struct TerminalElement {
    view: Entity<TerminalView>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = Theme::global(cx).clone();
        let base_font = font(FONT_MONO);
        // The canvas zooms by scaling the rem size; scale the cells with it.
        let zoom = window.rem_size() / px(16.);
        let font_id = window.text_system().resolve_font(&base_font);
        let base_cell_width = window
            .text_system()
            .advance(font_id, px(FONT_SIZE), 'm')
            .map(|advance| advance.width)
            .unwrap_or(px(7.8));
        let font_size = px(FONT_SIZE * zoom);
        let line_height = px(LINE_HEIGHT * zoom);
        let cell_width = base_cell_width * zoom;

        // Fit the grid to the space we got. Bounds and cells scale together,
        // so zooming doesn't change the grid (the epsilon absorbs float
        // error that would otherwise flip a column at some zoom levels).
        let grid = GridSize {
            columns: ((bounds.size.width / cell_width + 1e-3).floor() as usize).max(2),
            lines: ((bounds.size.height / line_height + 1e-3).floor() as usize).max(1),
        };
        let unzoomed_cell = size(base_cell_width, px(LINE_HEIGHT));
        self.view.update(cx, |view, _| view.resize(grid, unzoomed_cell));

        let view = self.view.read(cx);
        let focused = view.focus_handle.is_focused(window);
        let Ok(session) = &view.session else {
            return TerminalPrepaint {
                line_height,
                backgrounds: Vec::new(),
                lines: Vec::new(),
                cursor: None,
            };
        };
        let term = session.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let default_bg = theme.background;
        let cell_origin = |row: usize, column: usize| {
            point(
                bounds.origin.x + cell_width * column as f32,
                bounds.origin.y + line_height * row as f32,
            )
        };

        let mut backgrounds: Vec<(usize, usize, usize, Hsla)> = Vec::new();
        let mut batches: Vec<Batch> = Vec::new();
        let mut current: Option<Batch> = None;

        for indexed in content.display_iter {
            let cell = indexed.cell;
            let row = (indexed.point.line.0 + offset) as usize;
            let column = indexed.point.column.0;
            if row >= grid.lines {
                continue;
            }
            // The second half of a wide character: nothing to draw, but the
            // next run must start at its own column.
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                batches.extend(current.take());
                continue;
            }

            let mut fg = resolve_color(cell.fg, content.colors, &theme);
            let mut bg = resolve_color(cell.bg, content.colors, &theme);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = fg.opacity(0.66);
            }
            if cell.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            let cells = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };

            if bg != default_bg {
                match backgrounds.last_mut() {
                    Some((r, start, len, color))
                        if *r == row && *start + *len == column && *color == bg =>
                    {
                        *len += cells
                    }
                    _ => backgrounds.push((row, column, cells, bg)),
                }
            }

            let style = CellStyle {
                color: fg,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                undercurl: cell.flags.contains(Flags::UNDERCURL),
                strikethrough: cell.flags.contains(Flags::STRIKEOUT),
            };
            let decorated = style.underline || style.strikethrough;
            if cell.c == ' ' && !decorated {
                // Blank cells look the same in any style, so they extend the
                // current run instead of splitting it.
                if let Some(batch) = &mut current
                    && batch.row == row
                    && batch.column + batch.cells == column
                {
                    batch.text.push(' ');
                    batch.cells += 1;
                } else {
                    batches.extend(current.take());
                }
                continue;
            }

            match &mut current {
                Some(batch)
                    if batch.row == row
                        && batch.column + batch.cells == column
                        && batch.style == style =>
                {
                    batch.text.push(cell.c);
                    batch.cells += cells;
                }
                _ => {
                    batches.extend(current.take());
                    current = Some(Batch {
                        row,
                        column,
                        cells,
                        text: cell.c.to_string(),
                        style,
                    });
                }
            }
            // A run can't continue past a wide character: glyphs are placed
            // one cell apart.
            if cells == 2 {
                batches.extend(current.take());
            }
        }
        batches.extend(current.take());

        let text_system = window.text_system().clone();
        let shape_text = |text: &str, style: &CellStyle| -> ShapedLine {
            let font = Font {
                weight: if style.bold {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                },
                style: if style.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                },
                ..base_font.clone()
            };
            let run = TextRun {
                len: text.len(),
                font,
                color: style.color,
                background_color: None,
                underline: style.underline.then_some(UnderlineStyle {
                    color: Some(style.color),
                    thickness: px(1.),
                    wavy: style.undercurl,
                }),
                strikethrough: style.strikethrough.then_some(StrikethroughStyle {
                    color: Some(style.color),
                    thickness: px(1.),
                }),
            };
            // `force_width` puts every glyph on the cell grid.
            text_system.shape_line(
                SharedString::from(text.to_string()),
                font_size,
                &[run],
                Some(cell_width),
            )
        };

        let lines = batches
            .iter()
            .filter_map(|batch| {
                // Runs that are only blank cells draw nothing.
                let text = batch.text.trim_end();
                (!text.is_empty())
                    .then(|| (cell_origin(batch.row, batch.column), shape_text(text, &batch.style)))
            })
            .collect();

        let backgrounds = backgrounds
            .into_iter()
            .map(|(row, column, cells, color)| {
                (
                    Bounds::new(cell_origin(row, column), size(cell_width * cells as f32, line_height)),
                    color,
                )
            })
            .collect();

        // The cursor, unless it's hidden or scrolled out of view.
        let cursor_row = content.cursor.point.line.0 + offset;
        let cursor = (content.cursor.shape != CursorShape::Hidden
            && (0..grid.lines as i32).contains(&cursor_row))
        .then(|| {
            let row = cursor_row as usize;
            let column = content.cursor.point.column.0;
            let under = &term.grid()[content.cursor.point];
            let cells = if under.flags.contains(Flags::WIDE_CHAR) { 2. } else { 1. };
            let bounds = Bounds::new(cell_origin(row, column), size(cell_width * cells, line_height));
            let color = resolve_color(AnsiColor::Named(NamedColor::Cursor), content.colors, &theme);
            // Unfocused terminals show a hollow block, like most terminals.
            let shape = if focused {
                content.cursor.shape
            } else {
                CursorShape::HollowBlock
            };
            let glyph = (shape == CursorShape::Block && under.c != ' ').then(|| {
                shape_text(
                    &under.c.to_string(),
                    &CellStyle {
                        color: default_bg,
                        bold: under.flags.contains(Flags::BOLD),
                        italic: under.flags.contains(Flags::ITALIC),
                        underline: false,
                        undercurl: false,
                        strikethrough: false,
                    },
                )
            });
            PaintCursor {
                bounds,
                shape,
                color,
                glyph,
            }
        });

        TerminalPrepaint {
            line_height,
            backgrounds,
            lines,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        let line_height = prepaint.line_height;
        // Beam and underline cursors: 2px at 100%, never thinner than 1px.
        let stroke = (line_height / LINE_HEIGHT * 2.).max(px(1.));
        window.paint_layer(bounds, |window| {
            for (bounds, color) in prepaint.backgrounds.drain(..) {
                window.paint_quad(fill(bounds, color));
            }
            for (origin, line) in prepaint.lines.drain(..) {
                line.paint(origin, line_height, window, cx).ok();
            }
            if let Some(cursor) = prepaint.cursor.take() {
                let b = cursor.bounds;
                match cursor.shape {
                    CursorShape::Block => {
                        window.paint_quad(fill(b, cursor.color));
                        if let Some(glyph) = cursor.glyph {
                            glyph.paint(b.origin, line_height, window, cx).ok();
                        }
                    }
                    CursorShape::Beam => {
                        window.paint_quad(fill(Bounds::new(b.origin, size(stroke, b.size.height)), cursor.color));
                    }
                    CursorShape::Underline => {
                        let origin = point(b.origin.x, b.bottom() - stroke);
                        window.paint_quad(fill(Bounds::new(origin, size(b.size.width, stroke)), cursor.color));
                    }
                    CursorShape::HollowBlock => {
                        window.paint_quad(outline(b, cursor.color, gpui::BorderStyle::Solid));
                    }
                    CursorShape::Hidden => {}
                }
                self.view.update(cx, |view, _| view.cursor_bounds = Some(b));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use alacritty_terminal::index::{Column as GridColumn, Line};

    use crate::services::server::tests::shell_command;

    #[test]
    fn dropped_paths_are_quoted_for_the_shell() {
        assert_eq!(super::shell_quote("/tmp/a.txt"), "/tmp/a.txt");
        assert_eq!(super::shell_quote("/tmp/my file.txt"), "'/tmp/my file.txt'");
        assert_eq!(super::shell_quote("it's"), "'it'\\''s'");
    }
    use gpui::Modifiers;

    use super::*;

    fn keystroke(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: None,
        }
    }

    fn sequence(key: &str, modifiers: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
        key_sequence(&keystroke(key, modifiers), mode).map(|bytes| bytes.into_owned())
    }

    #[test]
    fn special_keys_send_control_sequences() {
        let none = Modifiers::default();
        let plain = TermMode::empty();
        assert_eq!(sequence("enter", none, plain), Some(b"\r".to_vec()));
        assert_eq!(sequence("backspace", none, plain), Some(b"\x7f".to_vec()));
        assert_eq!(sequence("escape", none, plain), Some(b"\x1b".to_vec()));
        assert_eq!(sequence("up", none, plain), Some(b"\x1b[A".to_vec()));
        // Full-screen apps switch the arrows to application mode.
        assert_eq!(sequence("up", none, TermMode::APP_CURSOR), Some(b"\x1bOA".to_vec()));
        assert_eq!(sequence("left", Modifiers::control(), plain), Some(b"\x1b[1;5D".to_vec()));
        assert_eq!(sequence("left", Modifiers::alt(), plain), Some(b"\x1bb".to_vec()));
        assert_eq!(sequence("tab", Modifiers::shift(), plain), Some(b"\x1b[Z".to_vec()));
        assert_eq!(sequence("pagedown", none, plain), Some(b"\x1b[6~".to_vec()));
        assert_eq!(sequence("f5", none, plain), Some(b"\x1b[15~".to_vec()));
        assert_eq!(sequence("left", Modifiers::command(), plain), Some(b"\x01".to_vec()));
    }

    #[test]
    fn control_letters_send_c0_codes() {
        let plain = TermMode::empty();
        assert_eq!(sequence("c", Modifiers::control(), plain), Some(vec![0x03]));
        assert_eq!(sequence("d", Modifiers::control(), plain), Some(vec![0x04]));
        assert_eq!(sequence("[", Modifiers::control(), plain), Some(vec![0x1b]));
        assert_eq!(sequence("space", Modifiers::control(), plain), Some(vec![0x00]));
    }

    #[test]
    fn text_keys_are_left_to_the_input_handler() {
        let plain = TermMode::empty();
        assert_eq!(sequence("a", Modifiers::default(), plain), None);
        assert_eq!(sequence("a", Modifiers::shift(), plain), None);
        // ⌥ types characters on macOS (e.g. ⌥2 is ™ or @ on some layouts).
        assert_eq!(sequence("2", Modifiers::alt(), plain), None);
        // ⌘ shortcuts belong to the app.
        assert_eq!(sequence("t", Modifiers::command(), plain), None);
    }

    #[test]
    fn palette_covers_the_xterm_cube_and_ramp() {
        let theme = Theme::dark();
        let hex = |color: Hsla| {
            let rgb = hsla_to_rgb(color);
            (rgb.r, rgb.g, rgb.b)
        };
        assert_eq!(hex(color_for_index(16, &theme)), (0, 0, 0));
        assert_eq!(hex(color_for_index(196, &theme)), (255, 0, 0));
        assert_eq!(hex(color_for_index(231, &theme)), (255, 255, 255));
        assert_eq!(hex(color_for_index(232, &theme)), (8, 8, 8));
        assert_eq!(hex(color_for_index(255, &theme)), (238, 238, 238));
        // Named colors resolve to the theme.
        assert_eq!(
            hex(color_for_index(NamedColor::Background as usize, &theme)),
            hex(theme.background)
        );
    }

    /// The visible screen as text, one line per row, trailing blanks trimmed.
    fn screen(term: &Term<Listener>) -> String {
        let grid = term.grid();
        (0..grid.screen_lines() as i32)
            .map(|line| {
                let row = &grid[Line(line)];
                (0..grid.columns())
                    .map(|column| row[GridColumn(column)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wait_for(session: &Session, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if screen(&session.term.lock()).contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn prompt_marks_are_found_even_when_split() {
        let mut marks = MarkScanner::default();
        assert_eq!(marks.scan(b"hello"), None);
        assert_eq!(marks.scan(b"\x1b]133;A\x07$ "), Some(ShellState::Prompt));
        // The last mark in a chunk wins.
        assert_eq!(marks.scan(b"\x1b]133;D\x07\x1b]133;C\x07"), Some(ShellState::Running));
        // Split across reads, at every possible point.
        let mark = b"out\x1b]133;D\x07";
        for split in 1..mark.len() {
            let mut marks = MarkScanner::default();
            let first = marks.scan(&mark[..split]);
            let second = marks.scan(&mark[split..]);
            assert_eq!(first.or(second), Some(ShellState::Prompt), "split at {split}");
            assert!(first.is_none() || second.is_none(), "counted twice at {split}");
        }
    }

    fn wait_for_state(session: &Session, state: ShellState) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if session.shell_state() == state {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// The shell's prompt marks tell the app when keys should go straight
    /// to a running program.
    #[test]
    fn knows_when_a_command_is_running() {
        let host = crate::services::server::tests::start_test_host("shell-state");
        let (session, _events) = Session::attach("state-test", None, &host).expect("shell starts");
        assert!(wait_for_state(&session, ShellState::Prompt), "never reached a prompt");
        session.write(b"sleep 1\r".to_vec());
        assert!(wait_for_state(&session, ShellState::Running), "didn't see the command start");
        assert!(wait_for_state(&session, ShellState::Prompt), "didn't see it finish");
    }

    /// End to end: a real login shell on a PTY, through alacritty's parser.
    #[test]
    fn runs_a_shell() {
        // Starts in the folder it was given.
        let folder = std::env::temp_dir().join(format!("xharness-shell-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let host = crate::services::server::tests::start_test_host("terminal");
        let (mut session, mut events) =
            Session::attach("terminal-test", Some(folder.clone()), &host).expect("shell starts");
        let name = folder.file_name().unwrap().to_string_lossy().to_string();

        // Our own startup files run, not yours: our prompt, and (with zsh;
        // Linux without it gets bash) no oh-my-zsh, whose `omz` command
        // would exist if ~/.zshrc had loaded.
        let prompt = if cfg!(windows) { format!("{name} >") } else { format!("{name} ❯") };
        assert!(
            wait_for(&session, &prompt),
            "not our prompt; screen was:\n{}",
            screen(&session.term.lock())
        );
        if cfg!(unix) && std::path::Path::new("/bin/zsh").exists() {
            session.write(b"echo omz-$(whence -w omz)\r".to_vec());
            assert!(
                wait_for(&session, "omz-omz: none"),
                "your ~/.zshrc loaded; screen was:\n{}",
                screen(&session.term.lock())
            );
        }
        // The prompt reports the folder as the window title.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut titled = false;
        while !titled && Instant::now() < deadline {
            while let Ok(event) = events.try_recv() {
                titled |= matches!(&event, SessionEvent::Term(TermEvent::Title(title)) if title.ends_with(&name));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(titled, "no folder title");

        session.write(shell_command("echo in-$(basename \"$PWD\")\r", "echo \"in-$(Split-Path -Leaf $PWD)\"\r").to_vec());
        let expected = format!("in-{}", folder.file_name().unwrap().to_string_lossy());
        assert!(
            wait_for(&session, &expected),
            "wrong folder; screen was:\n{}",
            screen(&session.term.lock())
        );

        // The arithmetic only appears in the output if the shell ran it.
        session.write(shell_command("echo xharness-$((40 + 2))\r", "echo \"xharness-$(40 + 2)\"\r").to_vec());
        assert!(
            wait_for(&session, "xharness-42"),
            "no output; screen was:\n{}",
            screen(&session.term.lock())
        );

        // A resize reaches the shell (TIOCSWINSZ, or ConPTY's resize), not
        // just the grid.
        session.resize(
            GridSize {
                columns: 101,
                lines: 31,
            },
            size(px(8.), px(17.)),
        );
        session.write(
            shell_command(
                "echo size-$(tput cols)x$(tput lines)\r",
                "echo \"size-$($Host.UI.RawUI.WindowSize.Width)x$($Host.UI.RawUI.WindowSize.Height)\"\r",
            )
            .to_vec(),
        );
        assert!(
            wait_for(&session, "size-101x31"),
            "resize not seen; screen was:\n{}",
            screen(&session.term.lock())
        );
    }
}
