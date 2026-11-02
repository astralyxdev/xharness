//! A terminal window: your login shell on a PTY, emulated by
//! `alacritty_terminal` (the engine Zed's terminal uses) and drawn with GPUI.
//!
//! - [`Session`] owns the PTY and the emulator. alacritty's I/O thread reads
//!   the shell's output into the grid and reports events over a channel.
//! - [`TerminalView`] is the GPUI view: focus, keyboard and IME input,
//!   scrolling, the mouse (selecting text, or reporting it to programs that
//!   read it), copy and paste. Every key goes to the shell, as in any
//!   terminal.
//! - [`TerminalElement`] paints the grid: background runs, the selection,
//!   text runs shaped on a fixed cell grid, and the cursor. It sizes itself
//!   from the rem size around it, so a zoomed canvas scales the terminal
//!   without reflowing it.
//!
//! Not yet: hyperlinks, and block (⌥-drag) selection.

use std::{
    borrow::Cow,
    collections::VecDeque,
    net::Shutdown,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point as GridPoint, Side},
    selection::{Selection, SelectionRange, SelectionType},
    sync::FairMutex,
    term::{self, Term, TermMode, cell::Flags, cell::LineLength, color::Colors, viewport_to_point},
    vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb},
};

use crate::i18n::{t, tf};
use crate::services::server::{self, Hello, HostAddress, Stream, frame};
use futures::{StreamExt, channel::mpsc};
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, DispatchPhase, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, ExternalPaths, FocusHandle, Focusable, Font, FontStyle,
    FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla, KeyBinding, KeyDownEvent, Keystroke,
    LayoutId, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollWheelEvent, ShapedLine, SharedString, Size, Style, StrikethroughStyle, Task, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, font, outline, point, prelude::*,
    px, relative, rems, size,
};

use crate::ui::theme::{FONT_MONO, Mode, Theme};

actions!(terminal, [Paste, Copy, SelectAll]);

const KEY_CONTEXT: &str = "Terminal";
/// At 100% zoom; the canvas scales these.
const FONT_SIZE: f32 = 13.;
/// 1.3 × the font size, rounded so rows land on whole pixels.
const LINE_HEIGHT: f32 = 17.;
const SCROLLBACK_LINES: usize = 10_000;

/// ⌘C / ⌘V / ⌘A on macOS. Elsewhere ctrl-shift-C/V/A as in Linux
/// terminals, plus ctrl-C and ctrl-V as in Windows Terminal: ctrl-C copies
/// when something is selected and is ^C otherwise. ctrl-A stays ^A.
pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-v", Paste, context),
        KeyBinding::new("secondary-c", Copy, context),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([KeyBinding::new("cmd-a", SelectAll, context)]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-v", Paste, context),
        KeyBinding::new("ctrl-shift-c", Copy, context),
        KeyBinding::new("ctrl-shift-a", SelectAll, context),
    ]);
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
    /// What goes to the host, sent in order by a writer thread.
    outbox: Arc<Outbox>,
    /// The connection, to hang up when the view lets go.
    socket: Stream,
    size: GridSize,
    /// What the shell is doing, from its prompt marks (see [`ShellState`]).
    shell: Arc<AtomicU8>,
}

/// Input bigger than this goes to the host in several frames, so a huge
/// paste stays under the host's frame limit and a kill can cut it short.
const INPUT_CHUNK: usize = 16 << 10;

/// Frames on their way to the host. A writer thread sends them, so the UI
/// never waits on the socket: the host writes input to the shell as fast as
/// the shell reads it, and a big paste into a busy program can take a while.
#[derive(Default)]
struct Outbox {
    state: Mutex<OutboxState>,
    wake: Condvar,
}

#[derive(Default)]
struct OutboxState {
    frames: VecDeque<(u8, Vec<u8>)>,
    /// The kind of frame the writer is sending right now.
    sending: Option<u8>,
    /// The view let go: send what's left (a kill), then hang up.
    closed: bool,
}

impl Outbox {
    fn push(&self, kind: u8, payload: Vec<u8>) {
        self.state.lock().unwrap().frames.push_back((kind, payload));
        self.wake.notify_one();
    }

    fn push_input(&self, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        for chunk in bytes.chunks(INPUT_CHUNK) {
            state.frames.push_back((frame::INPUT, chunk.to_vec()));
        }
        self.wake.notify_one();
    }

    /// Queues a kill. Input still waiting (the rest of a paste) is moot.
    fn kill(&self) {
        let mut state = self.state.lock().unwrap();
        state.frames.retain(|(kind, _)| *kind != frame::INPUT);
        state.frames.push_back((frame::KILL, Vec::new()));
        self.wake.notify_one();
    }

    /// No more frames. Whether a kill is still to be sent (the writer hangs
    /// up after it); anything else waiting is dropped.
    fn close(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.frames.retain(|(kind, _)| *kind == frame::KILL);
        let killing = !state.frames.is_empty() || state.sending == Some(frame::KILL);
        self.wake.notify_one();
        killing
    }

    /// The next frame to send, waiting for one; `None` once closed and
    /// empty.
    fn next(&self) -> Option<(u8, Vec<u8>)> {
        let mut state = self.state.lock().unwrap();
        state.sending = None;
        loop {
            if let Some(frame) = state.frames.pop_front() {
                state.sending = Some(frame.0);
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            state = self.wake.wait(state).unwrap();
        }
    }
}

/// Sends the outbox's frames until the view lets go (or the host does).
fn send_frames(mut stream: Stream, outbox: Arc<Outbox>) {
    while let Some((kind, payload)) = outbox.next() {
        if server::write_frame(&mut stream, kind, &payload).is_err() {
            return;
        }
    }
    // Hanging up also wakes the reader thread.
    stream.shutdown(Shutdown::Both).ok();
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
            .map_err(|error| tf("Couldn't reach the server: {}", &[&error]))?;
        let clone = || {
            stream
                .try_clone()
                .map_err(|error| tf("Couldn't read from the terminal: {}", &[&error]))
        };
        let (reader, writer) = (clone()?, clone()?);
        let shell = Arc::new(AtomicU8::new(ShellState::Starting as u8));
        let outbox = Arc::new(Outbox::default());
        {
            let (term, shell) = (term.clone(), shell.clone());
            std::thread::Builder::new()
                .name("terminal reader".into())
                .spawn(move || read_output(reader, term, shell, replaying, events))
                .map_err(|error| tf("Couldn't start the terminal: {}", &[&error]))?;
        }
        {
            let outbox = outbox.clone();
            std::thread::Builder::new()
                .name("terminal writer".into())
                .spawn(move || send_frames(writer, outbox))
                .map_err(|error| tf("Couldn't start the terminal: {}", &[&error]))?;
        }

        Ok((
            Self {
                term,
                outbox,
                socket: stream,
                size,
                shell,
            },
            receiver,
        ))
    }

    fn shell_state(&self) -> ShellState {
        ShellState::from_u8(self.shell.load(Ordering::SeqCst))
    }

    /// Sends input to the shell. It's queued, so this never blocks.
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.outbox.push_input(&bytes.into());
    }

    fn resize(&mut self, size: GridSize, cell: Size<Pixels>) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.term.lock().resize(size);
        let window = window_size(size, cell);
        self.outbox.push(frame::RESIZE, server::encode_size(window).to_vec());
    }

    /// Ends the shell (closing the window, not quitting the app).
    fn kill(&self) {
        self.outbox.kill();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Detach: hang up, which also wakes the reader thread. A kill on its
        // way goes first; the writer hangs up after sending it.
        if !self.outbox.close() {
            self.socket.shutdown(Shutdown::Both).ok();
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
    /// Where the grid was last painted, to tell programs which cell the
    /// pointer is over.
    cells: Option<CellGeometry>,
    /// IME composition in progress, if any.
    marked_text: Option<String>,
    /// Where the cursor was last painted, for placing the IME window.
    cursor_bounds: Option<Bounds<Pixels>>,
    /// What the mouse is doing while a button is down.
    gesture: Option<Gesture>,
    /// The cell the last motion report named: moves within a cell aren't
    /// reported again.
    reported_cell: Option<(usize, usize)>,
    /// Where the pointer is while dragging a selection, for scrolling while
    /// it's past the top or bottom.
    drag_position: Option<Point<Pixels>>,
    /// Scrolls while a selection is dragged past an edge.
    autoscroll: Option<Task<()>>,
    /// Interrupts a running command for [`TerminalView::cd_into`].
    _interrupting: Option<Task<()>>,
    _events: Option<Task<()>>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

/// What a press of a mouse button turned into.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Gesture {
    /// The click that focused this terminal. It selects only if you drag,
    /// and it isn't reported to programs.
    Focus { origin: Point<Pixels> },
    /// Selecting text: dragging moves the selection's end.
    Select,
    /// A program reads the mouse: it gets the button's release, and drags
    /// if it asked for them. `code` is the press's button code.
    Report { button: MouseButton, code: u8 },
}

/// Mouse buttons 4 and 5, as xterm reports the wheel.
const WHEEL_UP: u8 = 64;
const WHEEL_DOWN: u8 = 65;
/// Added to a button code for motion; 3 is "no button".
const MOTION: u8 = 32;
const NO_BUTTON: u8 = 3;

/// xterm's code for a button (left 0, middle 1, right 2).
fn button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    }
}

/// The modifier bits of a mouse report. Shift isn't among them: shift
/// makes the mouse select instead of reporting.
fn modifier_code(modifiers: &Modifiers) -> u8 {
    (if modifiers.alt { 8 } else { 0 }) + (if modifiers.control { 16 } else { 0 })
}

/// The grid's place on screen.
#[derive(Clone, Copy, Debug)]
struct CellGeometry {
    origin: Point<Pixels>,
    width: Pixels,
    height: Pixels,
    columns: usize,
    lines: usize,
}

impl CellGeometry {
    /// The cell (column, line) under `position`, from 0, kept inside the
    /// grid.
    fn cell_at(&self, position: Point<Pixels>) -> (usize, usize) {
        let column = ((position.x - self.origin.x) / self.width).floor().max(0.) as usize;
        let line = ((position.y - self.origin.y) / self.height).floor().max(0.) as usize;
        (column.min(self.columns.saturating_sub(1)), line.min(self.lines.saturating_sub(1)))
    }

    /// The grid point under `position` (in the scrollback when it's scrolled
    /// back by `display_offset`) and which half of its cell the pointer is
    /// in, for selecting. Past an edge: the nearest cell.
    fn selection_point(&self, position: Point<Pixels>, display_offset: usize) -> (GridPoint, Side) {
        let (column, line) = self.cell_at(position);
        let x = (position.x - self.origin.x) / self.width;
        let side = if x >= self.columns as f32 || x - column as f32 > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        (viewport_to_point(display_offset, GridPoint::new(line, Column(column))), side)
    }

    /// How many lines to scroll while a selection is dragged to `position`:
    /// positive above the grid (back into the scrollback), negative below,
    /// 0 inside. Further out scrolls faster.
    fn lines_past_edge(&self, position: Point<Pixels>) -> i32 {
        let top = self.origin.y;
        let bottom = top + self.height * self.lines as f32;
        let lines = |distance: Pixels| ((distance / self.height).ceil() as i32).clamp(1, 10);
        if position.y < top {
            lines(top - position.y)
        } else if position.y >= bottom {
            -lines(position.y - bottom + px(1.))
        } else {
            0
        }
    }
}

/// A mouse event for the program, in the encoding it asked for: SGR
/// (`ESC[<b;x;yM`, `m` for a release), UTF-8, or xterm's original bytes
/// (which can't say columns or lines past 223; those are reported at the
/// edge, and a release doesn't say which button).
fn mouse_report(button: u8, column: usize, line: usize, pressed: bool, mode: TermMode) -> Vec<u8> {
    let (x, y) = (column + 1, line + 1);
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if pressed { 'M' } else { 'm' };
        return format!("\x1b[<{button};{x};{y}{end}").into_bytes();
    }
    let button = if pressed { button } else { (button & !0b11) | NO_BUTTON };
    let mut report = b"\x1b[M".to_vec();
    report.push(32 + button);
    for value in [x, y] {
        if mode.contains(TermMode::UTF8_MOUSE) {
            let code = char::from_u32((32 + value).min(2047) as u32).unwrap_or(' ');
            let mut buffer = [0u8; 4];
            report.extend_from_slice(code.encode_utf8(&mut buffer).as_bytes());
        } else {
            report.push((32 + value).min(255) as u8);
        }
    }
    report
}

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
            cells: None,
            marked_text: None,
            cursor_bounds: None,
            gesture: None,
            reported_cell: None,
            drag_position: None,
            autoscroll: None,
            _interrupting: None,
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
        self.gesture = None;
        self.autoscroll = None;
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
                self.session = Err(t("This terminal is open in another xharness window.").into());
                cx.notify();
            }
            SessionEvent::Disconnected => {
                // The host is gone (killed, or crashed). Attach again: that
                // starts a new host, and a fresh shell in the last folder.
                let recently = self
                    .reconnected_at
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(5));
                if recently {
                    self.session = Err(t("Lost the terminal session host.").into());
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

    /// Sends user input, jumping back to the live screen first. Typing
    /// clears the selection, as in other terminals.
    fn input(&mut self, bytes: impl Into<Cow<'static, [u8]>>, cx: &mut Context<Self>) {
        if let Ok(session) = &self.session {
            {
                let mut term = session.term.lock();
                term.scroll_display(Scroll::Bottom);
                term.selection = None;
            }
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

    /// Types `paths` at the prompt, quoted for the shell and separated by
    /// spaces, with a space after, without running anything (what dropping
    /// files on Terminal does). It goes in as a paste, so programs that take
    /// pastes (Claude Code, for one) see one.
    pub fn insert_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let mut text = paths.iter().map(|path| quote_path(path)).collect::<Vec<_>>().join(" ");
        text.push(' ');
        self.paste_text(&text, cx);
    }

    /// Takes the shell to `folder`, as if you typed `cd` there. A command
    /// that's running is interrupted first: ^C (again every half second;
    /// some programs, Claude Code for one, want two), then ^\ as a last try.
    /// The `cd` is typed once the shell is back at its prompt, or after the
    /// last try, unless a full-screen program (vim, say) still has the
    /// screen: keys typed there would edit something.
    pub fn cd_into(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let command = cd_command(&folder);
        if !self.running() {
            self._interrupting = None;
            self.input(command.into_bytes(), cx);
            return;
        }
        self.input(Interrupt::INTERRUPT, cx);
        let mut interrupt = Interrupt::new(Instant::now());
        self._interrupting = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Interrupt::POLL).await;
                let step = this.update(cx, |this, _| {
                    let step = interrupt.step(Instant::now(), this.running());
                    if let InterruptStep::Send(bytes) = step {
                        this.write(bytes);
                    }
                    step
                });
                match step {
                    Ok(InterruptStep::Done) => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
            this.update(cx, |this, cx| {
                if this.running() && this.mode().contains(TermMode::ALT_SCREEN) {
                    return;
                }
                this.input(command.into_bytes(), cx);
            })
            .ok();
        }));
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.paste_text(&text, cx);
    }

    /// Sends `text` as a paste: line breaks as returns, and bracketed when
    /// the program asked (so a shell shows it without running it). The
    /// selection goes, and the screen jumps to the bottom. The writer
    /// thread sends it, so a big paste doesn't hold up the window.
    fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        self.input(paste_bytes(text, bracketed), cx);
    }

    /// Copies the selection. With nothing selected, ⌘C does nothing; on
    /// other systems the key goes on, so ctrl-C is ^C.
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.selected_text() else {
            if !cfg!(target_os = "macos") {
                cx.propagate();
            }
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        // Like Windows Terminal: once copied, ctrl-C is ^C again.
        if !cfg!(target_os = "macos") {
            self.clear_selection(cx);
        }
    }

    /// Selects everything, the scrollback included.
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if let Ok(session) = &self.session {
            let mut term = session.term.lock();
            term.selection = Some(select_all(&term));
            cx.notify();
        }
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<String> {
        let session = self.session.as_ref().ok()?;
        session.term.lock().selection_to_string().filter(|text| !text.is_empty())
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if let Ok(session) = &self.session {
            session.term.lock().selection = None;
            cx.notify();
        }
    }

    // Mouse --------------------------------------------------------------------

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(cells), Ok(_)) = (self.cells, &self.session) else {
            return;
        };
        self.autoscroll = None;
        self.drag_position = None;
        // The click that picks this terminal (another window had the
        // keyboard) leaves the selection and the program alone.
        if !self.focus_handle.is_focused(window) {
            self.gesture = (event.button == MouseButton::Left).then_some(Gesture::Focus { origin: event.position });
            return;
        }
        let mode = self.mode();
        // Programs that read the mouse get it, unless shift is down (as in
        // iTerm and xterm).
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            let Some(button) = button_code(event.button) else {
                return;
            };
            let code = button + modifier_code(&event.modifiers);
            let (column, line) = cells.cell_at(event.position);
            self.write(mouse_report(code, column, line, true, mode));
            self.reported_cell = Some((column, line));
            self.gesture = Some(Gesture::Report { button: event.button, code });
            if event.button == MouseButton::Left {
                self.clear_selection(cx);
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        // Shift-click extends the selection; a double click selects words,
        // a triple click lines.
        let extend = event.modifiers.shift && event.click_count <= 1;
        self.select_at(event.position, event.click_count, extend, cx);
        self.gesture = Some(Gesture::Select);
    }

    /// Starts a selection at `position` (or moves the end of the one there
    /// is, if `extend`).
    fn select_at(&mut self, position: Point<Pixels>, click_count: usize, extend: bool, cx: &mut Context<Self>) {
        let (Some(cells), Ok(session)) = (self.cells, &self.session) else {
            return;
        };
        let mut term = session.term.lock();
        let (point, side) = cells.selection_point(position, term.grid().display_offset());
        match term.selection.as_mut() {
            Some(selection) if extend => selection.update(point, side),
            _ => term.selection = Some(Selection::new(selection_type(click_count), point, side)),
        }
        cx.notify();
    }

    /// Moves the end of the selection to `position`.
    fn drag_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(cells), Ok(session)) = (self.cells, &self.session) else {
            return;
        };
        let mut term = session.term.lock();
        let (point, side) = cells.selection_point(position, term.grid().display_offset());
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, side);
            cx.notify();
        }
    }

    /// The pointer moved, anywhere in the window (`hovered`: over this
    /// terminal).
    fn mouse_move(&mut self, event: &MouseMoveEvent, hovered: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cells) = self.cells else {
            return;
        };
        match self.gesture {
            Some(Gesture::Select) => {
                // The release happened somewhere we didn't hear about.
                if event.pressed_button != Some(MouseButton::Left) {
                    self.end_selection(cx);
                    return;
                }
                self.drag_position = Some(event.position);
                self.drag_selection(event.position, cx);
                if cells.lines_past_edge(event.position) != 0 && self.autoscroll.is_none() {
                    self.start_autoscroll(cx);
                }
            }
            Some(Gesture::Focus { origin }) => {
                if event.pressed_button != Some(MouseButton::Left) {
                    self.gesture = None;
                    return;
                }
                let moved = event.position - origin;
                if f32::from(moved.x.abs()).max(f32::from(moved.y.abs())) > 3. {
                    // A drag after all: select from where it started.
                    self.select_at(origin, 1, false, cx);
                    self.gesture = Some(Gesture::Select);
                    self.mouse_move(event, hovered, window, cx);
                }
            }
            Some(Gesture::Report { code, .. }) => {
                if event.pressed_button.is_none() {
                    self.gesture = None;
                    return;
                }
                let mode = self.mode();
                if mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
                    self.report_motion(code + MOTION, event.position, mode);
                }
            }
            None => {
                // Programs that asked for every move get them while the
                // pointer is over a terminal you're typing in.
                let mode = self.mode();
                if hovered
                    && mode.contains(TermMode::MOUSE_MOTION)
                    && event.pressed_button.is_none()
                    && !event.modifiers.shift
                    && !cx.has_active_drag()
                    && self.focus_handle.is_focused(window)
                {
                    let code = NO_BUTTON + MOTION + modifier_code(&event.modifiers);
                    self.report_motion(code, event.position, mode);
                }
            }
        }
    }

    /// Reports a move to the program, if it reached another cell.
    fn report_motion(&mut self, code: u8, position: Point<Pixels>, mode: TermMode) {
        let Some(cells) = self.cells else {
            return;
        };
        let cell = cells.cell_at(position);
        if self.reported_cell != Some(cell) {
            self.reported_cell = Some(cell);
            self.write(mouse_report(code, cell.0, cell.1, true, mode));
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.gesture {
            Some(Gesture::Report { button, code }) if button == event.button => {
                self.gesture = None;
                let mode = self.mode();
                if let Some(cells) = self.cells
                    && mode.intersects(TermMode::MOUSE_MODE)
                {
                    let (column, line) = cells.cell_at(event.position);
                    self.write(mouse_report(code, column, line, false, mode));
                }
            }
            Some(Gesture::Select) if event.button == MouseButton::Left => self.end_selection(cx),
            Some(Gesture::Focus { .. }) if event.button == MouseButton::Left => self.gesture = None,
            _ => {}
        }
    }

    /// The button's up: a click that didn't drag leaves nothing selected.
    fn end_selection(&mut self, cx: &mut Context<Self>) {
        self.gesture = None;
        self.autoscroll = None;
        self.drag_position = None;
        if let Ok(session) = &self.session {
            let mut term = session.term.lock();
            if term.selection.as_ref().is_some_and(|selection| selection.is_empty()) {
                term.selection = None;
            }
        }
        cx.notify();
    }

    /// While a selection is dragged past the top or bottom, scrolls (and
    /// keeps selecting) until the pointer comes back or the button's up.
    fn start_autoscroll(&mut self, cx: &mut Context<Self>) {
        self.autoscroll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(50)).await;
                let scrolling = this.update(cx, |this, cx| this.autoscroll_step(cx));
                if !matches!(scrolling, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn autoscroll_step(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some(Gesture::Select), Some(position), Some(cells), Ok(session)) =
            (self.gesture, self.drag_position, self.cells, &self.session)
        else {
            self.autoscroll = None;
            return false;
        };
        let lines = cells.lines_past_edge(position);
        if lines == 0 {
            self.autoscroll = None;
            return false;
        }
        session.term.lock().scroll_display(Scroll::Delta(lines));
        self.drag_selection(position, cx);
        cx.notify();
        true
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
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            // A program that reads the mouse (Claude Code, vim with
            // mouse=a, htop) gets the wheel itself and scrolls what it
            // shows. Shift-scrolling still scrolls the terminal, as in
            // other terminals.
            let (column, line) = self.cells.map_or((0, 0), |cells| cells.cell_at(event.position));
            let direction = if lines > 0 { WHEEL_UP } else { WHEEL_DOWN };
            let button = direction + modifier_code(&event.modifiers);
            let report = mouse_report(button, column, line, true, mode);
            self.write(report.repeat(lines.unsigned_abs() as usize));
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
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
            // Scrolling while dragging a selection keeps it under the
            // pointer.
            if self.gesture == Some(Gesture::Select) {
                self.drag_selection(event.position, cx);
            }
            cx.notify();
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        // Over a program that reads the mouse, the pointer is an arrow.
        let cursor = if self.mode().intersects(TermMode::MOUSE_MODE) {
            CursorStyle::Arrow
        } else {
            CursorStyle::IBeam
        };

        div()
            .id("terminal")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .px_2()
            .py_1()
            .cursor(cursor)
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::select_all))
            .on_key_down(cx.listener(Self::key_down))
            // On the padding too, so a click there picks the nearest cell.
            // Moves and releases are heard by the grid (see
            // `TerminalElement::paint`), wherever the pointer goes.
            .on_any_mouse_down(cx.listener(Self::mouse_down))
            // Files dropped from Finder type their paths, as in Terminal.
            // Files dragged from an Explorer go on to the canvas.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.insert_paths(paths.paths(), cx)))
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

// Selection ------------------------------------------------------------------

/// A single click selects characters, a double click words, a triple click
/// lines.
fn selection_type(click_count: usize) -> SelectionType {
    match click_count {
        0 | 1 => SelectionType::Simple,
        2 => SelectionType::Semantic,
        _ => SelectionType::Lines,
    }
}

/// Everything: the scrollback's first line to the last one with text (the
/// empty screen below it would only copy as blank lines).
fn select_all<T>(term: &Term<T>) -> Selection {
    let top = term.topmost_line();
    let mut bottom = term.bottommost_line();
    while bottom > top && term.grid()[bottom].line_length() == Column(0) {
        bottom = Line(bottom.0 - 1);
    }
    let mut selection = Selection::new(SelectionType::Simple, GridPoint::new(top, Column(0)), Side::Left);
    selection.update(GridPoint::new(bottom, term.last_column()), Side::Right);
    selection
}

/// The selected cells of each visible row: (row, first column, last
/// column), for painting.
fn selection_rows(
    range: &SelectionRange,
    display_offset: usize,
    lines: usize,
    columns: usize,
) -> Vec<(usize, usize, usize)> {
    let last_column = columns.saturating_sub(1);
    (0..lines)
        .filter_map(|row| {
            let line = Line(row as i32 - display_offset as i32);
            if line < range.start.line || line > range.end.line {
                return None;
            }
            let (first, last) = if range.is_block {
                (range.start.column.0, range.end.column.0)
            } else {
                (
                    if line == range.start.line { range.start.column.0 } else { 0 },
                    if line == range.end.line { range.end.column.0 } else { last_column },
                )
            };
            let last = last.min(last_column);
            (first <= last).then_some((row, first, last))
        })
        .collect()
}

// Pasting and typing for the shell -------------------------------------------

/// What a paste sends: line breaks (CRLF or LF) as returns, which is what
/// the Return key sends, and, when the program turned bracketed paste on,
/// the text between `ESC[200~` and `ESC[201~`. Brackets inside the text are
/// taken out, so it can't end the paste early and run what follows.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if !bracketed {
        return text.into_bytes();
    }
    format!("{PASTE_START}{}{PASTE_END}", strip_paste_brackets(text)).into_bytes()
}

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// `text` with no bracketed-paste markers in it, including ones that only
/// appear once another is taken out (`ESC[20` `ESC[201~` `1~`).
fn strip_paste_brackets(mut text: String) -> String {
    while text.contains(PASTE_START) || text.contains(PASTE_END) {
        text = text.replace(PASTE_START, "").replace(PASTE_END, "");
    }
    text
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

/// `text` as one PowerShell argument: as is when it's only safe characters,
/// else in single quotes (where only `'` is special, written twice).
fn powershell_quote(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_-./\\:".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "''"))
    }
}

/// A path, quoted for this system's shell.
fn quote_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) { powershell_quote(&text) } else { shell_quote(&text) }
}

/// What [`TerminalView::cd_into`] types: clear the line, then go to
/// `folder`. The leading space keeps it out of history where the shell
/// skips such lines.
fn cd_command(folder: &Path) -> String {
    let folder = folder.to_string_lossy();
    if cfg!(windows) { powershell_cd(&folder) } else { posix_cd(&folder) }
}

/// ^E ^U (end of line, then delete to its start: the whole line in zsh and
/// bash), then `cd`.
fn posix_cd(folder: &str) -> String {
    format!("\x05\x15 cd -- '{}'\r", folder.replace('\'', "'\\''"))
}

/// ctrl-End, ctrl-Home (PSReadLine deletes to the end, then to the start),
/// then `Set-Location`.
fn powershell_cd(folder: &str) -> String {
    format!("\x1b[1;5F\x1b[1;5H Set-Location -LiteralPath '{}'\r", folder.replace('\'', "''"))
}

/// How [`TerminalView::cd_into`] gets a running command to stop.
struct Interrupt {
    started: Instant,
    last_sent: Instant,
    quit_sent: bool,
}

#[derive(Debug, PartialEq)]
enum InterruptStep {
    /// Still running; wait.
    Wait,
    /// Still running; send this.
    Send(&'static [u8]),
    /// At a prompt (or given up): type the command.
    Done,
}

impl Interrupt {
    /// How often to look.
    const POLL: Duration = Duration::from_millis(100);
    /// ^C again this often.
    const RESEND: Duration = Duration::from_millis(500);
    /// Then the last try.
    const QUIT_AFTER: Duration = Duration::from_secs(3);
    /// Then give up.
    const GIVE_UP_AFTER: Duration = Duration::from_secs(4);
    const INTERRUPT: &'static [u8] = b"\x03";
    /// ^\ (SIGQUIT). Windows has no such signal; ^C once more there.
    const QUIT: &'static [u8] = if cfg!(windows) { b"\x03" } else { b"\x1c" };

    /// Right after the first ^C went.
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            last_sent: now,
            quit_sent: false,
        }
    }

    fn step(&mut self, now: Instant, running: bool) -> InterruptStep {
        let elapsed = now.duration_since(self.started);
        if !running || elapsed >= Self::GIVE_UP_AFTER {
            return InterruptStep::Done;
        }
        if self.quit_sent {
            return InterruptStep::Wait;
        }
        if elapsed >= Self::QUIT_AFTER {
            self.quit_sent = true;
            return InterruptStep::Send(Self::QUIT);
        }
        if now.duration_since(self.last_sent) >= Self::RESEND {
            self.last_sent = now;
            return InterruptStep::Send(Self::INTERRUPT);
        }
        InterruptStep::Wait
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
    /// Where the pointer counts as over the terminal.
    hitbox: Hitbox,
    line_height: Pixels,
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    /// The selection's cells, row by row, and its color.
    selection: Vec<Bounds<Pixels>>,
    selection_color: Hsla,
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
        let cells = CellGeometry {
            origin: bounds.origin,
            width: cell_width,
            height: line_height,
            columns: grid.columns,
            lines: grid.lines,
        };
        self.view.update(cx, |view, _| {
            view.resize(grid, unzoomed_cell);
            view.cells = Some(cells);
        });

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let view = self.view.read(cx);
        let focused = view.focus_handle.is_focused(window);
        // Dimmer while another window has the keyboard, as in macOS.
        let selection_color = theme.tints.blue.base.opacity(if focused { 0.3 } else { 0.15 });
        let Ok(session) = &view.session else {
            return TerminalPrepaint {
                hitbox,
                line_height,
                backgrounds: Vec::new(),
                selection: Vec::new(),
                selection_color,
                lines: Vec::new(),
                cursor: None,
            };
        };
        let term = session.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let selected = content.selection;
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

        let selection = selected
            .map(|range| selection_rows(&range, content.display_offset, grid.lines, grid.columns))
            .unwrap_or_default()
            .into_iter()
            .map(|(row, first, mut last)| {
                // A wide character the selection ends on is selected whole.
                let line = Line(row as i32 - offset);
                if last + 1 < grid.columns
                    && term.grid()[GridPoint::new(line, Column(last))].flags.contains(Flags::WIDE_CHAR)
                {
                    last += 1;
                }
                let cells = (last - first + 1) as f32;
                Bounds::new(cell_origin(row, first), size(cell_width * cells, line_height))
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
            hitbox,
            line_height,
            backgrounds,
            selection,
            selection_color,
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

        // Moves and releases anywhere in the window: a drag that started
        // here keeps selecting (or reporting) when the pointer leaves.
        // Presses are the view's own (see `TerminalView::mouse_down`).
        let (view, hitbox) = (self.view.clone(), prepaint.hitbox.clone());
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let hovered = hitbox.is_hovered(window);
            if hovered || view.read(cx).gesture.is_some() {
                view.update(cx, |view, cx| view.mouse_move(event, hovered, window, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && view.read(cx).gesture.is_some() {
                view.update(cx, |view, cx| view.mouse_up(event, window, cx));
            }
        });

        let line_height = prepaint.line_height;
        // Beam and underline cursors: 2px at 100%, never thinner than 1px.
        let stroke = (line_height / LINE_HEIGHT * 2.).max(px(1.));
        window.paint_layer(bounds, |window| {
            for (bounds, color) in prepaint.backgrounds.drain(..) {
                window.paint_quad(fill(bounds, color));
            }
            // Over the cells' colors, under the text.
            for bounds in prepaint.selection.drain(..) {
                window.paint_quad(fill(bounds, prepaint.selection_color));
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

    /// Programs that read the mouse get the wheel in the encoding they
    /// asked for, at the cell under the pointer.
    #[test]
    fn the_wheel_is_reported_to_programs_that_read_the_mouse() {
        use alacritty_terminal::term::TermMode;
        use super::{CellGeometry, WHEEL_DOWN, WHEEL_UP, mouse_report};
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(mouse_report(WHEEL_UP, 0, 0, true, sgr), b"\x1b[<64;1;1M");
        assert_eq!(mouse_report(WHEEL_DOWN, 79, 23, true, sgr), b"\x1b[<65;80;24M");
        // xterm's bytes: 32 + button, 32 + column, 32 + line.
        assert_eq!(mouse_report(WHEEL_UP, 9, 4, true, TermMode::MOUSE_REPORT_CLICK), b"\x1b[M\x60\x2a\x25");
        let utf8 = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        assert_eq!(mouse_report(WHEEL_DOWN, 299, 0, true, utf8), "\x1b[Ma\u{14c}!".as_bytes());

        let cells = CellGeometry {
            origin: gpui::point(gpui::px(10.), gpui::px(20.)),
            width: gpui::px(8.),
            height: gpui::px(16.),
            columns: 80,
            lines: 24,
        };
        assert_eq!(cells.cell_at(gpui::point(gpui::px(10.), gpui::px(20.))), (0, 0));
        assert_eq!(cells.cell_at(gpui::point(gpui::px(35.), gpui::px(52.))), (3, 2));
        // Outside the grid: its nearest edge.
        assert_eq!(cells.cell_at(gpui::point(gpui::px(0.), gpui::px(9000.))), (0, 23));
    }

    /// Clicks, releases and drags, for programs that read the mouse.
    #[test]
    fn buttons_are_reported_to_programs_that_read_the_mouse() {
        use alacritty_terminal::term::TermMode;
        use super::{MOTION, NO_BUTTON, button_code, modifier_code, mouse_report};
        let left = button_code(gpui::MouseButton::Left).unwrap();
        let right = button_code(gpui::MouseButton::Right).unwrap();
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        // SGR says which button was released, with a lowercase `m`.
        assert_eq!(mouse_report(left, 4, 2, true, sgr), b"\x1b[<0;5;3M");
        assert_eq!(mouse_report(left, 4, 2, false, sgr), b"\x1b[<0;5;3m");
        assert_eq!(mouse_report(right, 0, 0, false, sgr), b"\x1b[<2;1;1m");
        // A drag adds 32; moving with no button down is 3 + 32.
        assert_eq!(mouse_report(left + MOTION, 6, 2, true, sgr), b"\x1b[<32;7;3M");
        assert_eq!(mouse_report(NO_BUTTON + MOTION, 0, 0, true, sgr), b"\x1b[<35;1;1M");
        // ⌥ and ctrl ride along; xterm's own encoding releases "button 3".
        let modifiers = gpui::Modifiers { alt: true, control: true, ..Default::default() };
        assert_eq!(modifier_code(&modifiers), 24);
        let normal = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(mouse_report(right + 8, 0, 0, true, normal), b"\x1b[M\x2a\x21\x21");
        assert_eq!(mouse_report(right + 8, 0, 0, false, normal), b"\x1b[M\x2b\x21\x21");
    }

    #[test]
    fn dropped_paths_are_quoted_for_the_shell() {
        assert_eq!(super::shell_quote("/tmp/a.txt"), "/tmp/a.txt");
        assert_eq!(super::shell_quote("/tmp/my file.txt"), "'/tmp/my file.txt'");
        assert_eq!(super::shell_quote("it's"), "'it'\\''s'");
        assert_eq!(super::powershell_quote("C:\\Users\\me\\a.txt"), "C:\\Users\\me\\a.txt");
        assert_eq!(super::powershell_quote("C:\\my files\\it's"), "'C:\\my files\\it''s'");
        // PowerShell reads these specially outside quotes.
        assert_eq!(super::powershell_quote("a,b$c"), "'a,b$c'");
    }

    /// `cd_into` clears what's typed, then changes folder, quoted so any
    /// name works.
    #[test]
    fn cd_commands_are_quoted_for_the_shell() {
        assert_eq!(super::posix_cd("/tmp/it's here"), "\x05\x15 cd -- '/tmp/it'\\''s here'\r");
        assert_eq!(
            super::powershell_cd("C:\\it's here"),
            "\x1b[1;5F\x1b[1;5H Set-Location -LiteralPath 'C:\\it''s here'\r"
        );
    }

    #[test]
    fn pasted_line_breaks_are_returns() {
        use super::paste_bytes;
        assert_eq!(paste_bytes("one\r\ntwo\nthree\r", false), b"one\rtwo\rthree\r");
        assert_eq!(paste_bytes("no breaks", false), b"no breaks");
    }

    /// A program that turned bracketed paste on gets the text between the
    /// markers, and text can't end the paste early to run what follows.
    #[test]
    fn bracketed_pastes_cant_end_early() {
        use super::paste_bytes;
        assert_eq!(paste_bytes("ls\nrm -rf x\n", true), b"\x1b[200~ls\rrm -rf x\r\x1b[201~");
        assert_eq!(paste_bytes("a\x1b[201~\nrm x\n", true), b"\x1b[200~a\rrm x\r\x1b[201~");
        assert_eq!(paste_bytes("\x1b[200~a\x1b[201~", true), b"\x1b[200~a\x1b[201~");
        // Taking one marker out mustn't leave another behind.
        assert_eq!(paste_bytes("x\x1b[20\x1b[201~1~y", true), b"\x1b[200~xy\x1b[201~");
    }

    /// When a command is running, `cd_into` sends ^C right away, again every
    /// half second, ^\ after three seconds, and gives up a second later.
    #[test]
    fn interrupting_escalates_then_gives_up() {
        use super::{Interrupt, InterruptStep};
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut interrupt = Interrupt::new(start);
        assert_eq!(interrupt.step(at(100), true), InterruptStep::Wait);
        assert_eq!(interrupt.step(at(500), true), InterruptStep::Send(b"\x03"));
        assert_eq!(interrupt.step(at(600), true), InterruptStep::Wait);
        assert_eq!(interrupt.step(at(1000), true), InterruptStep::Send(b"\x03"));
        assert_eq!(interrupt.step(at(3000), true), InterruptStep::Send(Interrupt::QUIT));
        assert_eq!(interrupt.step(at(3500), true), InterruptStep::Wait);
        assert_eq!(interrupt.step(at(4000), true), InterruptStep::Done);
        // Back at a prompt: type at once.
        let mut interrupt = Interrupt::new(start);
        assert_eq!(interrupt.step(at(100), false), InterruptStep::Done);
    }

    /// Frames go out in order; a kill drops input that hasn't gone yet (the
    /// rest of a paste) and is still sent when the view lets go.
    #[test]
    fn the_outbox_sends_in_order_and_kills_first() {
        use super::{INPUT_CHUNK, Outbox};
        let frames = |outbox: &Outbox| -> Vec<(u8, usize)> {
            outbox.state.lock().unwrap().frames.iter().map(|(kind, bytes)| (*kind, bytes.len())).collect()
        };
        let outbox = Outbox::default();
        outbox.push_input(&vec![b'a'; INPUT_CHUNK * 2 + 1]);
        outbox.push(frame::RESIZE, vec![1]);
        assert_eq!(
            frames(&outbox),
            [(frame::INPUT, INPUT_CHUNK), (frame::INPUT, INPUT_CHUNK), (frame::INPUT, 1), (frame::RESIZE, 1)]
        );
        assert_eq!(outbox.next().map(|(kind, _)| kind), Some(frame::INPUT));
        outbox.kill();
        assert_eq!(frames(&outbox), [(frame::RESIZE, 1), (frame::KILL, 0)]);
        assert!(outbox.close(), "the kill is still to go");
        assert_eq!(outbox.next().map(|(kind, _)| kind), Some(frame::KILL));
        assert_eq!(outbox.next(), None);

        // Detaching drops what's left.
        let outbox = Outbox::default();
        outbox.push_input(b"ls\r");
        assert!(!outbox.close());
        assert_eq!(outbox.next(), None);
    }

    /// A small screen of text, as the shell would draw it.
    fn term_with(text: &str, columns: usize, lines: usize) -> Term<alacritty_terminal::event::VoidListener> {
        let config = term::Config {
            scrolling_history: 100,
            ..Default::default()
        };
        let mut term = Term::new(config, &GridSize { columns, lines }, alacritty_terminal::event::VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, text.as_bytes());
        term
    }

    /// Where a click or drag lands on a grid of 10×20 px cells at (0, 0):
    /// `column` with a fraction for where in the cell.
    fn at(cells: &CellGeometry, column: f32, line: usize) -> (GridPoint, Side) {
        cells.selection_point(point(px(column * 10.), px(line as f32 * 20. + 5.)), 0)
    }

    fn cells(columns: usize, lines: usize) -> CellGeometry {
        CellGeometry {
            origin: point(px(0.), px(0.)),
            width: px(10.),
            height: px(20.),
            columns,
            lines,
        }
    }

    #[test]
    fn mouse_selection_copies_the_text_under_it() {
        let mut term = term_with("hello world\r\nsecond line\r\n$ ", 20, 4);
        let cells = cells(20, 4);

        // A drag from the left half of "h" to the right half of "o".
        let (start, side) = at(&cells, 0.2, 0);
        assert_eq!((start, side), (GridPoint::new(Line(0), Column(0)), Side::Left));
        let mut selection = Selection::new(selection_type(1), start, side);
        let (end, side) = at(&cells, 4.7, 0);
        selection.update(end, side);
        term.selection = Some(selection);
        assert_eq!(term.selection_to_string().as_deref(), Some("hello"));

        // Across lines, past the end of the second: the break between them
        // comes, the one after the last doesn't (alacritty drops it).
        let (end, side) = at(&cells, 19.9, 1);
        term.selection.as_mut().unwrap().update(end, side);
        assert_eq!(term.selection_to_string().as_deref(), Some("hello world\nsecond line"));

        // A click without a drag selects nothing.
        let (start, side) = at(&cells, 3.2, 1);
        term.selection = Some(Selection::new(selection_type(1), start, side));
        assert!(term.selection.as_ref().unwrap().is_empty());
        assert_eq!(term.selection_to_string(), None);

        // Double click: the word; triple click: the line.
        let (point, side) = at(&cells, 8.5, 0);
        term.selection = Some(Selection::new(selection_type(2), point, side));
        assert_eq!(term.selection_to_string().as_deref(), Some("world"));
        term.selection = Some(Selection::new(selection_type(3), point, side));
        assert_eq!(term.selection_to_string().as_deref(), Some("hello world\n"));

        // ⌘A: everything, down to the prompt.
        term.selection = Some(select_all(&term));
        assert_eq!(term.selection_to_string().as_deref(), Some("hello world\nsecond line\n$"));
    }

    /// Output that scrolls the screen moves the selection with its text,
    /// and ⌘A reaches back into the scrollback.
    #[test]
    fn the_selection_stays_on_its_text_as_output_arrives() {
        let mut term = term_with("first\r\nsecond\r\n", 10, 3);
        let mut selection = Selection::new(SelectionType::Simple, GridPoint::new(Line(1), Column(0)), Side::Left);
        selection.update(GridPoint::new(Line(1), Column(5)), Side::Right);
        term.selection = Some(selection);
        assert_eq!(term.selection_to_string().as_deref(), Some("second"));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"three\r\nfour\r\nfive\r\n");
        assert!(term.history_size() > 0, "the text scrolled into the scrollback");
        assert_eq!(term.selection_to_string().as_deref(), Some("second"));

        term.selection = Some(select_all(&term));
        assert_eq!(term.selection_to_string().as_deref(), Some("first\nsecond\nthree\nfour\nfive"));
    }

    /// The highlight covers each selected row's cells: from the start to the
    /// end of the line, whole lines between, up to the end on the last.
    #[test]
    fn the_selection_is_painted_row_by_row() {
        let range = SelectionRange::new(GridPoint::new(Line(-1), Column(3)), GridPoint::new(Line(1), Column(2)), false);
        // Scrolled back by one: line -1 is the top row.
        assert_eq!(selection_rows(&range, 1, 4, 10), [(0, 3, 9), (1, 0, 9), (2, 0, 2)]);
        // At the bottom, line -1 is off screen.
        assert_eq!(selection_rows(&range, 0, 4, 10), [(0, 0, 9), (1, 0, 2)]);
    }

    /// Dragging past the top scrolls back, past the bottom forward; faster
    /// further out.
    #[test]
    fn dragging_past_an_edge_scrolls() {
        let cells = CellGeometry {
            origin: point(px(0.), px(100.)),
            ..cells(20, 4)
        };
        assert_eq!(cells.lines_past_edge(point(px(5.), px(150.))), 0);
        assert_eq!(cells.lines_past_edge(point(px(5.), px(95.))), 1);
        assert_eq!(cells.lines_past_edge(point(px(5.), px(50.))), 3);
        assert_eq!(cells.lines_past_edge(point(px(5.), px(180.))), -1);
        assert_eq!(cells.lines_past_edge(point(px(5.), px(9000.))), -10);
        // Beyond the right edge counts as the last cell's right half.
        let (at, side) = cells.selection_point(point(px(500.), px(105.)), 0);
        assert_eq!((at, side), (GridPoint::new(Line(0), Column(19)), Side::Right));
    }

    /// End to end with a real shell: interrupt what's running the way
    /// `cd_into` does (a command that ignores ^C, on Unix, so it takes the
    /// ^\), then go to a folder whose name needs quoting.
    #[test]
    fn cd_into_interrupts_a_running_command() {
        let folder = std::env::temp_dir().join(format!("xharness cd 'here' {}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let host = crate::services::server::tests::start_test_host("cd-into");
        let (session, _events) = Session::attach("cd-into-test", None, &host).expect("shell starts");
        assert!(wait_for_state(&session, ShellState::Prompt), "never reached a prompt");
        session.write(shell_command("trap '' INT; sleep 30\r", "sleep 30\r").to_vec());
        assert!(wait_for_state(&session, ShellState::Running), "didn't see the command start");

        session.write(Interrupt::INTERRUPT);
        let mut interrupt = Interrupt::new(Instant::now());
        loop {
            std::thread::sleep(Interrupt::POLL);
            match interrupt.step(Instant::now(), session.shell_state() == ShellState::Running) {
                InterruptStep::Send(bytes) => session.write(bytes),
                InterruptStep::Wait => {}
                InterruptStep::Done => break,
            }
        }
        assert_eq!(session.shell_state(), ShellState::Prompt, "still running");
        session.write(cd_command(&folder).into_bytes());
        session.write(shell_command("echo \"in:$(basename \"$PWD\")\"\r", "echo \"in:$(Split-Path -Leaf $PWD)\"\r").to_vec());
        let expected = format!("in:{}", folder.file_name().unwrap().to_string_lossy());
        assert!(
            wait_for(&session, &expected),
            "not in the folder; screen was:\n{}",
            screen(&session.term.lock())
        );
        std::fs::remove_dir_all(&folder).ok();
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
