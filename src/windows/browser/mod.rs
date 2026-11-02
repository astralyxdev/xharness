//! Browser windows: a real web page (WebKit, see `platform/webview.rs`) with
//! back, forward, reload and an address bar, shown at the window's size or
//! at a device's (iPhone, iPad, laptop…, see [`devices`]) to check how it
//! looks there. Mobile devices also tell sites they're mobile browsers.
//!
//! - The address bar says how safe the connection is (a lock for https,
//!   "Not secure" for plain http away from this machine), and suggests
//!   pages from the history of every Browser window ([`history`]).
//! - ⌘F finds text in the page; the ⋯ menu opens the page elsewhere,
//!   copies its address or a screenshot, zooms it, reloads it without the
//!   cache and opens Web Inspector.
//! - The console drawer lists what the page logs and its uncaught errors
//!   ([`console`]); the toolbar counts the errors.
//!
//! A device's page is always its own size (times the page zoom): resizing
//! the window never changes how big its text is. When it doesn't fit, the
//! area around it scrolls.
//!
//! Pages show on macOS; elsewhere the window offers to open the page in
//! the system's browser instead.

pub mod address;
pub mod console;
pub mod devices;
pub mod history;
pub mod layers;

use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use gpui::{
    AnyElement, AnyWindowHandle, App, Bounds, ClickEvent, ClipboardItem, Context, Corners, DragMoveEvent, ElementId,
    Empty, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, MouseButton, MouseDownEvent,
    Pixels, Point, RenderImage, ScrollHandle, ScrollWheelEvent, SharedString, Subscription, Task, Window, actions,
    canvas, div, fill, outline, point, prelude::*, px, relative, rems, size,
};

use crate::i18n::{t, tf, tn};
use crate::services::projects::BrowserSettings;
use crate::ui::context_menu::MenuAt;
use crate::ui::{
    self,
    actions::{Cancel, Confirm},
    assets::IconName,
    context_menu::MenuEntry,
    text_input::TextInput,
    theme::{FONT_MONO, Theme},
};
use address::Security;
use console::{Console, Level};
use devices::{DEVICES, Kind, QUICK};
use history::{History, Visit};

#[cfg(target_os = "macos")]
use crate::platform::webview::{PageState, Shortcut, WebEvent, WebView};

actions!(
    browser,
    [FocusAddress, Reload, GoBack, GoForward, FindInPage, FindPrevious, SelectNext, SelectPrevious]
);

const KEY_CONTEXT: &str = "Browser";
/// How often the page's address, title and loading state are read.
#[cfg(target_os = "macos")]
const POLL_EVERY: Duration = Duration::from_millis(150);
/// Dev servers offered on a new window's start page.
const DEV_SERVERS: [&str; 3] = ["localhost:3000", "localhost:5173", "localhost:8080"];
/// How many recent pages a new window's start page lists.
const RECENT: usize = 5;
/// The page zoom's steps, in percent, as in Safari.
const ZOOM_STEPS: [u16; 12] = [50, 67, 75, 85, 100, 115, 125, 150, 175, 200, 250, 300];
/// How long a notice ("Screenshot copied") shows.
const NOTICE_FOR: Duration = Duration::from_secs(2);
/// The scrollbars' gutters, beside a device too big for the window, in rems.
const GUTTER: f32 = 0.625;

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("enter", Confirm, context),
        KeyBinding::new("escape", Cancel, context),
        // As in Safari.
        KeyBinding::new("secondary-l", FocusAddress, context),
        KeyBinding::new("secondary-r", Reload, context),
        KeyBinding::new("secondary-[", GoBack, context),
        KeyBinding::new("secondary-]", GoForward, context),
        KeyBinding::new("secondary-f", FindInPage, context),
        KeyBinding::new("shift-enter", FindPrevious, context),
        // Suggestions under the address bar.
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("up", SelectPrevious, context),
    ]);
}

/// Where the page is (read from WebKit; stays empty without a page).
#[cfg(not(target_os = "macos"))]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageState {
    pub url: String,
    pub title: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub loading: bool,
    pub progress: f64,
}

pub enum BrowserEvent {
    /// The page was clicked: this is the window you're working in now.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Activated,
    /// The page or its size changed, to save.
    Changed(BrowserSettings),
    /// A menu to show (the device list, the ⋯ menu).
    Menu {
        entries: Vec<MenuEntry>,
        position: MenuAt,
    },
}

/// The find bar, while it's open.
struct FindBar {
    input: Entity<TextInput>,
    /// What was searched for last, and whether it was found (`None`: not
    /// known yet).
    query: String,
    found: Option<bool>,
    _changed: Subscription,
}

/// Dragging a scrollbar of a device bigger than the window.
struct ScrollThumb {
    browser: EntityId,
    vertical: bool,
}

/// Drags are only used for their move events; nothing follows the pointer.
struct NoPreview;

impl Render for NoPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub struct BrowserView {
    /// Where its dropdown buttons are (see [`ui::Anchors`]).
    anchors: ui::Anchors,
    settings: BrowserSettings,
    address: Entity<TextInput>,
    address_focused: bool,
    /// The page's (GPUI's side of it: the native page takes the keyboard
    /// while this is focused).
    focus_handle: FocusHandle,
    page_focused: bool,
    state: PageState,
    /// A page that didn't load: its address and why.
    failed: Option<(String, String)>,
    /// How the page last looked, painted under it (and in its place while
    /// something covers it).
    snapshot: Option<Arc<RenderImage>>,
    /// Snapshots replaced since the last paint, to free.
    retired: Vec<Arc<RenderImage>>,
    /// Where the page was laid out at the last paint.
    layout: Rc<Cell<Option<devices::Layout>>>,
    /// The page area's size (less the scrollbars) at the last paint.
    viewport: Rc<Cell<(f32, f32)>>,
    /// How far the area around a device too big for the window is
    /// scrolled.
    scroll: (f32, f32),
    /// A scrollbar being dragged: where the pointer and the scroll started.
    thumb_drag: Option<(Point<Pixels>, (f32, f32))>,
    /// Whether this is the window you're working in (the canvas says, each
    /// frame): only then does the page take the mouse.
    active: Rc<Cell<bool>>,
    window: Option<AnyWindowHandle>,
    find: Option<FindBar>,
    console: Console,
    console_open: bool,
    console_scroll: ScrollHandle,
    /// The address bar's text that `suggestions` are for (`None`: it isn't
    /// being edited), and which one is picked with the arrow keys.
    typed: Option<String>,
    suggestions: Vec<Visit>,
    suggestion: Option<usize>,
    /// Where the address bar and the whole window were at the last paint,
    /// to put the suggestions under the address bar.
    address_bounds: Rc<Cell<Bounds<Pixels>>>,
    root_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// A short message in the device bar ("Screenshot copied").
    notice: Option<SharedString>,
    notice_task: Option<Task<()>>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    screenshot_task: Option<Task<()>>,
    #[cfg(target_os = "macos")]
    web: Option<Rc<WebView>>,
    _tasks: Vec<Task<()>>,
    _address_changed: Subscription,
}

impl EventEmitter<BrowserEvent> for BrowserView {}

impl Focusable for BrowserView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BrowserView {
    pub fn new(settings: BrowserSettings, cx: &mut Context<Self>) -> Self {
        let address = cx.new(|cx| TextInput::new(t("Search, or type an address: localhost:3000, example.com"), cx).inline());
        let text = address::display(&settings.url);
        address.update(cx, |input, cx| input.set_text(text, cx));
        // Typing in it updates the suggestions.
        let address_changed = cx.observe(&address, |_, _, cx| cx.notify());
        #[allow(unused_mut)]
        let mut this = Self {
            anchors: ui::Anchors::default(),
            settings,
            address,
            address_focused: false,
            focus_handle: cx.focus_handle(),
            page_focused: false,
            state: PageState::default(),
            failed: None,
            snapshot: None,
            retired: Vec::new(),
            layout: Rc::new(Cell::new(None)),
            viewport: Rc::new(Cell::new((0., 0.))),
            scroll: (0., 0.),
            thumb_drag: None,
            active: Rc::new(Cell::new(false)),
            window: None,
            find: None,
            console: Console::default(),
            console_open: false,
            console_scroll: ScrollHandle::new(),
            typed: None,
            suggestions: Vec::new(),
            suggestion: None,
            address_bounds: Rc::new(Cell::new(Bounds::default())),
            root_bounds: Rc::new(Cell::new(Bounds::default())),
            notice: None,
            notice_task: None,
            screenshot_task: None,
            #[cfg(target_os = "macos")]
            web: None,
            _tasks: Vec::new(),
            _address_changed: address_changed,
        };
        #[cfg(target_os = "macos")]
        this.start_page(cx);
        this
    }

    #[cfg(target_os = "macos")]
    fn start_page(&mut self, cx: &mut Context<Self>) {
        use futures::StreamExt;
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let Some(web) = WebView::new(sender) else {
            return;
        };
        web.set_user_agent(self.user_agent());
        if !self.settings.url.is_empty() {
            web.load(&self.settings.url);
        }
        self.web = Some(web);
        self._tasks.push(cx.spawn(async move |this, cx| {
            while let Some(event) = receiver.next().await {
                let window = this.read_with(cx, |this, _| this.window).ok().flatten();
                // A click focuses the page, and shortcuts focus a field:
                // both need the window.
                let needs_window = matches!(event, WebEvent::Clicked | WebEvent::Shortcut(_));
                let handled = match (window, needs_window) {
                    (Some(window), true) => cx
                        .update_window(window, |_, window, cx| {
                            this.update(cx, |this, cx| this.web_event(event, Some(window), cx))
                        })
                        .and_then(|result| result),
                    _ => this.update(cx, |this, cx| this.web_event(event, None, cx)),
                };
                if handled.is_err() {
                    break;
                }
            }
        }));
        self._tasks.push(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_EVERY).await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    pub fn settings(&self) -> &BrowserSettings {
        &self.settings
    }

    /// Whether this is the canvas's active window. Inactive, the page
    /// doesn't take clicks, scrolling or swipes: the first click picks the
    /// window, and scrolling over it pans the canvas, as over any window.
    pub fn set_active(&self, active: bool) {
        self.active.set(active);
    }

    /// The page gets the keyboard (or the address bar, with no page yet).
    pub fn focus(&self, window: &mut Window, cx: &App) {
        if self.settings.url.is_empty() {
            window.focus(&self.address.focus_handle(cx));
        } else {
            window.focus(&self.focus_handle);
        }
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
            || self.address.focus_handle(cx).contains_focused(window, cx)
            || self.find.as_ref().is_some_and(|find| find.input.focus_handle(cx).contains_focused(window, cx))
    }

    pub fn loading(&self) -> bool {
        self.state.loading
    }

    /// What the window's header shows: the page's title, or its address.
    pub fn summary(&self) -> Option<String> {
        if let Some((url, _)) = &self.failed {
            return Some(tf("can't open {}", &[&address::display(url)]));
        }
        if !self.state.title.trim().is_empty() {
            return Some(self.state.title.trim().to_string());
        }
        (!self.settings.url.is_empty()).then(|| address::display(&self.settings.url))
    }

    fn user_agent(&self) -> Option<&'static str> {
        self.settings.device.as_deref().and_then(devices::find).and_then(|device| device.user_agent)
    }

    /// Whether pages can show here (macOS, with WebKit).
    fn has_page(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.web.is_some();
        #[cfg(not(target_os = "macos"))]
        false
    }

    fn page_zoom(&self) -> f32 {
        f32::from(self.settings.page_zoom.unwrap_or(100)) / 100.
    }

    /// Shows `text` in the device bar for a moment.
    fn show_notice(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.notice = Some(text.into());
        self.notice_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_FOR).await;
            this.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // The page ---------------------------------------------------------------

    #[cfg(target_os = "macos")]
    fn web_event(&mut self, event: WebEvent, window: Option<&mut Window>, cx: &mut Context<Self>) {
        match event {
            WebEvent::Clicked => {
                if let Some(window) = window {
                    window.focus(&self.focus_handle);
                }
                cx.emit(BrowserEvent::Activated);
            }
            WebEvent::Committed => {
                // A new page: what the last one logged and found is gone.
                self.console.clear();
                if let Some(find) = &mut self.find {
                    find.found = None;
                }
            }
            WebEvent::Finished => {
                if let Some(web) = self.web.clone() {
                    // Fresh, for when something covers the page.
                    if web.is_shown() {
                        web.snapshot();
                    }
                    let state = web.state();
                    History::record(&state.url, &state.title, cx);
                }
            }
            WebEvent::Failed { url, message } => {
                let url = if url.is_empty() { self.settings.url.clone() } else { url };
                self.failed = Some((url, message));
            }
            WebEvent::Snapshot(image) => {
                if let Some(old) = self.snapshot.replace(image) {
                    self.retired.push(old);
                }
            }
            WebEvent::Screenshot(png) => {
                cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(gpui::ImageFormat::Png, png)));
                self.show_notice(t("Screenshot copied"), cx);
            }
            WebEvent::Console(message) => {
                self.console.push(message);
                if self.console_open {
                    self.console_scroll.scroll_to_bottom();
                }
            }
            WebEvent::Found(found) => {
                if let Some(find) = &mut self.find {
                    find.found = Some(found);
                }
            }
            WebEvent::Shortcut(shortcut) => {
                if let Some(window) = window {
                    match shortcut {
                        Shortcut::Find => self.open_find(window, cx),
                        Shortcut::FocusAddress => self.focus_address(&FocusAddress, window, cx),
                        Shortcut::Reload => self.reload(&Reload, window, cx),
                        Shortcut::Back => self.go_back(&GoBack, window, cx),
                        Shortcut::Forward => self.go_forward(&GoForward, window, cx),
                    }
                }
            }
        }
        cx.notify();
    }

    /// Reads where the page is now.
    #[cfg(target_os = "macos")]
    fn poll(&mut self, cx: &mut Context<Self>) {
        let Some(web) = &self.web else {
            return;
        };
        let state = web.state();
        if state == self.state {
            return;
        }
        let moved = state.url != self.state.url && !state.url.is_empty();
        // A title that arrives after the page finished loading.
        let titled = state.title != self.state.title && !state.title.trim().is_empty() && !state.loading;
        self.state = state;
        if moved {
            // A page loaded (a link, back, a redirect): it's what the
            // window shows now.
            self.failed = None;
            self.settings.url = self.state.url.clone();
            cx.emit(BrowserEvent::Changed(self.settings.clone()));
            if !self.address_focused {
                let text = address::display(&self.state.url);
                self.address.update(cx, |input, cx| input.set_text(text, cx));
            }
        }
        if titled {
            let (url, title) = (self.state.url.clone(), self.state.title.clone());
            History::record(&url, &title, cx);
        }
        cx.notify();
    }

    /// Opens an address or a file (a file dropped on the window).
    pub fn open(&mut self, address: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate(address, window, cx);
    }

    /// Opens what's typed in the address bar.
    fn navigate(&mut self, typed: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(url) = address::url_for(typed) else {
            return;
        };
        self.failed = None;
        self.settings.url = url.clone();
        self.typed = None;
        self.suggestions.clear();
        self.suggestion = None;
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            web.load(&url);
        }
        self.address.update(cx, |input, cx| input.set_text(address::display(&url), cx));
        window.focus(&self.focus_handle);
        cx.emit(BrowserEvent::Changed(self.settings.clone()));
        cx.notify();
    }

    fn reload(&mut self, _: &Reload, _: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            if self.failed.take().is_some() || self.state.url.is_empty() {
                // Nothing loaded: try the address again.
                if !self.settings.url.is_empty() {
                    web.load(&self.settings.url);
                }
            } else if self.state.loading {
                web.stop();
            } else {
                web.reload();
            }
        }
        cx.notify();
    }

    /// Reloads every file the page uses, not the ones cached.
    fn reload_from_origin(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            if self.failed.take().is_some() || self.state.url.is_empty() {
                if !self.settings.url.is_empty() {
                    web.load(&self.settings.url);
                }
            } else {
                web.reload_from_origin();
            }
        }
        cx.notify();
    }

    fn go_back(&mut self, _: &GoBack, _: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            self.failed = None;
            web.back();
        }
        cx.notify();
    }

    fn go_forward(&mut self, _: &GoForward, _: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            self.failed = None;
            web.forward();
        }
        cx.notify();
    }

    fn focus_address(&mut self, _: &FocusAddress, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.address.focus_handle(cx));
        self.address.update(cx, |input, cx| input.select_all_text(cx));
    }

    fn find_focused(&self, window: &Window, cx: &App) -> bool {
        self.find.as_ref().is_some_and(|find| find.input.focus_handle(cx).is_focused(window))
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_focused(window, cx) {
            self.find_next(false, cx);
            return;
        }
        if !self.address.focus_handle(cx).is_focused(window) {
            cx.propagate();
            return;
        }
        let typed = match self.suggestion.and_then(|ix| self.suggestions.get(ix)) {
            Some(visit) => visit.url.clone(),
            None => self.address.read(cx).text().to_string(),
        };
        self.navigate(&typed, window, cx);
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_focused(window, cx) {
            self.close_find(window, cx);
            return;
        }
        if !self.address.focus_handle(cx).is_focused(window) {
            cx.propagate();
            return;
        }
        // First the suggestions go, then the edit.
        if !self.suggestions.is_empty() {
            self.suggestions.clear();
            self.suggestion = None;
            cx.notify();
            return;
        }
        // Back to the page's own address.
        let text = address::display(&self.settings.url);
        self.address.update(cx, |input, cx| input.set_text(text, cx));
        self.typed = None;
        if !self.settings.url.is_empty() {
            window.focus(&self.focus_handle);
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_suggestion(1, window, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.move_suggestion(-1, window, cx);
    }

    fn move_suggestion(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.suggestions.is_empty() || !self.address.focus_handle(cx).is_focused(window) {
            cx.propagate();
            return;
        }
        let count = self.suggestions.len() as isize;
        self.suggestion = match self.suggestion {
            // Up from the first goes back to what's typed.
            Some(0) if by < 0 => None,
            Some(ix) => Some((ix as isize + by).clamp(0, count - 1) as usize),
            None if by > 0 => Some(0),
            None => Some(count as usize - 1),
        };
        cx.notify();
    }

    /// Recomputes the suggestions for what's in the address bar, while it's
    /// being edited (typed in, or empty with no page yet).
    fn update_suggestions(&mut self, cx: &mut Context<Self>) {
        let text = self.address.read(cx).text().to_string();
        let editing =
            self.address_focused && (self.settings.url.is_empty() || text != address::display(&self.settings.url));
        if !editing {
            self.typed = None;
            self.suggestions.clear();
            self.suggestion = None;
            return;
        }
        if self.typed.as_deref() == Some(text.as_str()) {
            return;
        }
        let current = self.settings.url.clone();
        self.suggestions = History::get(cx)
            .suggest(&text, history::SUGGESTIONS + 1)
            .into_iter()
            .filter(|visit| visit.url != current)
            .take(history::SUGGESTIONS)
            .collect();
        self.suggestion = None;
        self.typed = Some(text);
    }

    // Find in page -------------------------------------------------------------

    fn find_in_page(&mut self, _: &FindInPage, window: &mut Window, cx: &mut Context<Self>) {
        self.open_find(window, cx);
    }

    /// Opens the find bar (or selects what's in it) and gives it the
    /// keyboard.
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_page() || self.settings.url.is_empty() {
            return;
        }
        if self.find.is_none() {
            let input = cx.new(|cx| TextInput::new(t("Find in page"), cx).inline());
            let changed = cx.observe(&input, |this, _, cx| this.find_changed(cx));
            self.find = Some(FindBar { input, query: String::new(), found: None, _changed: changed });
        }
        if let Some(find) = &self.find {
            window.focus(&find.input.focus_handle(cx));
            find.input.update(cx, |input, cx| input.select_all_text(cx));
        }
        cx.notify();
    }

    /// Searches as you type.
    fn find_changed(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        let text = find.input.read(cx).text().to_string();
        if text == find.query {
            return;
        }
        find.query = text;
        find.found = None;
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            web.find(&find.query, false);
        }
        cx.notify();
    }

    fn find_next(&mut self, backwards: bool, cx: &mut Context<Self>) {
        let Some(find) = &self.find else {
            return;
        };
        #[cfg(target_os = "macos")]
        if let Some(web) = &self.web {
            web.find(&find.query, backwards);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (find, backwards);
        cx.notify();
    }

    fn find_previous(&mut self, _: &FindPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_focused(window, cx) {
            cx.propagate();
            return;
        }
        self.find_next(true, cx);
    }

    /// Closes the find bar; the page takes the keyboard back.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    // The ⋯ menu ---------------------------------------------------------------

    fn set_page_zoom(&mut self, percent: u16, cx: &mut Context<Self>) {
        let zoom = (percent != 100).then_some(percent);
        if zoom == self.settings.page_zoom {
            return;
        }
        self.settings.page_zoom = zoom;
        cx.emit(BrowserEvent::Changed(self.settings.clone()));
        cx.notify();
    }

    /// The next zoom step in (`up`) or out.
    fn step_zoom(&mut self, up: bool, cx: &mut Context<Self>) {
        let current = self.settings.page_zoom.unwrap_or(100);
        let next = if up {
            ZOOM_STEPS.iter().copied().find(|&step| step > current)
        } else {
            ZOOM_STEPS.iter().rev().copied().find(|&step| step < current)
        };
        if let Some(next) = next {
            self.set_page_zoom(next, cx);
        }
    }

    fn copy_address(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.settings.url.clone()));
        self.show_notice(t("Address copied"), cx);
    }

    /// Copies a screenshot of the page, once the menu that asked for it
    /// has stopped covering it.
    fn copy_screenshot(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            self.screenshot_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                this.update(cx, |this, _| {
                    if let Some(web) = &this.web {
                        web.screenshot();
                    }
                })
                .ok();
            }));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = cx;
    }

    fn toggle_console(&mut self, cx: &mut Context<Self>) {
        self.console_open = !self.console_open;
        if self.console_open {
            self.console_scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    fn more_menu(&self, position: MenuAt, cx: &mut Context<Self>) {
        let browser = cx.entity().downgrade();
        let act = |f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            let browser = browser.clone();
            move |window: &mut Window, cx: &mut App| {
                browser.update(cx, |this, cx| f(this, window, cx)).ok();
            }
        };
        let url = self.settings.url.clone();
        let mut entries = Vec::new();
        if !url.is_empty() {
            entries.push(MenuEntry::item(IconName::ExternalLink, t("Open in default browser"), move |_, cx| {
                cx.open_url(&url)
            }));
            entries.push(MenuEntry::item(IconName::Copy, t("Copy address"), act(Self::copy_address)));
            if self.has_page() {
                entries.push(MenuEntry::item(IconName::RefreshCw, t("Reload ignoring cache"), act(Self::reload_from_origin)));
            }
            entries.push(MenuEntry::Separator);
        }
        let zoom = self.settings.page_zoom.unwrap_or(100);
        entries.push(MenuEntry::item(IconName::ZoomIn, t("Zoom in"), act(|this, _, cx| this.step_zoom(true, cx))));
        entries.push(MenuEntry::item(IconName::ZoomOut, t("Zoom out"), act(|this, _, cx| this.step_zoom(false, cx))));
        entries.push(MenuEntry::check(
            if zoom == 100 { t("Actual size").to_string() } else { tf("Actual size (now {}%)", &[&zoom]) },
            zoom == 100,
            act(|this, _, cx| this.set_page_zoom(100, cx)),
        ));
        if self.has_page() && !self.settings.url.is_empty() {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::item(IconName::Search, t("Find in page…"), act(Self::open_find)));
            entries.push(MenuEntry::item(IconName::Camera, t("Copy screenshot"), act(Self::copy_screenshot)));
        }
        entries.push(MenuEntry::item(
            IconName::Bug,
            if self.console_open { t("Hide console") } else { t("Show console") },
            act(|this, _, cx| this.toggle_console(cx)),
        ));
        #[cfg(target_os = "macos")]
        if self.web.as_ref().is_some_and(|web| web.can_inspect()) && !self.settings.url.is_empty() {
            entries.push(MenuEntry::item(
                IconName::CodeXml,
                t("Show Web Inspector"),
                act(|this, _, _| {
                    if let Some(web) = &this.web {
                        web.show_inspector();
                    }
                }),
            ));
        }
        cx.emit(BrowserEvent::Menu { entries, position });
    }

    // The size ---------------------------------------------------------------

    fn set_device(&mut self, device: Option<&'static str>, cx: &mut Context<Self>) {
        let before = self.user_agent();
        self.settings.device = device.map(String::from);
        if device.is_none() {
            self.settings.landscape = false;
        }
        self.scroll = (0., 0.);
        #[cfg(target_os = "macos")]
        if self.user_agent() != before
            && let Some(web) = &self.web
        {
            // Sites pick their mobile or desktop page when it loads.
            web.set_user_agent(self.user_agent());
            if !self.state.url.is_empty() {
                web.reload();
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = before;
        cx.emit(BrowserEvent::Changed(self.settings.clone()));
        cx.notify();
    }

    fn rotate(&mut self, cx: &mut Context<Self>) {
        self.settings.landscape = !self.settings.landscape;
        self.scroll = (0., 0.);
        cx.emit(BrowserEvent::Changed(self.settings.clone()));
        cx.notify();
    }

    fn device_menu(&self, position: MenuAt, cx: &mut Context<Self>) {
        let browser = cx.entity().downgrade();
        let pick = |device: Option<&'static str>| {
            let browser = browser.clone();
            move |_: &mut Window, cx: &mut App| {
                browser.update(cx, |this, cx| this.set_device(device, cx)).ok();
            }
        };
        let current = self.settings.device.as_deref();
        let mut entries = vec![MenuEntry::check(t("Responsive (the window's size)"), current.is_none(), pick(None))];
        let mut kind = None;
        for device in DEVICES {
            if kind != Some(device.kind) {
                entries.push(MenuEntry::Separator);
                kind = Some(device.kind);
            }
            entries.push(MenuEntry::check(
                format!("{}   {} × {}", device.name, device.width, device.height),
                current == Some(device.id),
                pick(Some(device.id)),
            ));
        }
        cx.emit(BrowserEvent::Menu { entries, position });
    }

    /// Scrolling over the area around a device too big for the window
    /// (in the window you're working in; elsewhere it pans the canvas).
    fn scroll_area(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.get() else {
            return;
        };
        if !self.active.get() || layout.max_scroll == (0., 0.) {
            return;
        }
        cx.stop_propagation();
        let delta = event.delta.pixel_delta(px(20.));
        let next = (
            (self.scroll.0 - f32::from(delta.x)).clamp(0., layout.max_scroll.0),
            (self.scroll.1 - f32::from(delta.y)).clamp(0., layout.max_scroll.1),
        );
        if next != self.scroll {
            self.scroll = next;
            cx.notify();
        }
    }

    /// A scrollbar's thumb along one axis: where it starts and how long it
    /// is, in px along its track, and how far the area scrolls per px the
    /// thumb moves.
    fn thumb(layout: &devices::Layout, viewport: (f32, f32), vertical: bool) -> Option<(f32, f32, f32)> {
        let (track, page, scroll, max) = if vertical {
            (viewport.1, layout.height, layout.scroll.1, layout.max_scroll.1)
        } else {
            (viewport.0, layout.width, layout.scroll.0, layout.max_scroll.0)
        };
        if max <= 0. || track <= 0. {
            return None;
        }
        let content = page + 2. * devices::MARGIN * layout.zoom;
        let length = (track * track / content).clamp(24f32.min(track), track);
        let room = (track - length).max(1.);
        Some((scroll / max * room, length, max / room))
    }

    fn drag_thumb(&mut self, event: &DragMoveEvent<ScrollThumb>, cx: &mut Context<Self>) {
        let drag = event.drag(cx);
        if drag.browser != cx.entity_id() {
            return;
        }
        let vertical = drag.vertical;
        let (Some((start, from)), Some(layout)) = (self.thumb_drag, self.layout.get()) else {
            return;
        };
        let Some((_, _, per_px)) = Self::thumb(&layout, self.viewport.get(), vertical) else {
            return;
        };
        let moved = event.event.position - start;
        if vertical {
            self.scroll.1 = (from.1 + f32::from(moved.y) * per_px).clamp(0., layout.max_scroll.1);
        } else {
            self.scroll.0 = (from.0 + f32::from(moved.x) * per_px).clamp(0., layout.max_scroll.0);
        }
        cx.notify();
    }

    // Rendering --------------------------------------------------------------

    fn render_toolbar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let state = &self.state;
        let has_page = self.has_page();
        let nav = |id: &'static str, icon: IconName, enabled: bool| {
            let button = ui::small_icon_button(id, icon, theme);
            if enabled { button } else { button.opacity(0.35) }
        };
        let address_border = if self.address_focused { theme.border_active } else { theme.border };
        // Something shows as soon as it starts.
        let progress = state.loading.then_some((state.progress as f32).clamp(0.1, 1.));
        let muted = theme.muted_foreground;
        let (security_icon, security_color, warning) = if self.address_focused {
            (IconName::Search, muted, None)
        } else {
            match address::security(&self.settings.url) {
                Security::Secure => (IconName::Lock, muted, None),
                Security::NotSecure => (IconName::TriangleAlert, theme.tints.amber.base, Some(t("Not secure"))),
                Security::File => (IconName::File, muted, None),
                Security::Local | Security::Other => (IconName::Globe, muted, None),
            }
        };
        let address_bounds = self.address_bounds.clone();
        let errors = self.console.errors();

        div()
            .relative()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_0p5()
            .h_9()
            .px_1p5()
            .border_b_1()
            .border_color(theme.border)
            .child(nav("browser-back", IconName::ArrowLeft, has_page && state.can_go_back).when(
                has_page && state.can_go_back,
                |button| button.on_click(cx.listener(|this, _, window, cx| this.go_back(&GoBack, window, cx))),
            ))
            .child(nav("browser-forward", IconName::ArrowRight, has_page && state.can_go_forward).when(
                has_page && state.can_go_forward,
                |button| button.on_click(cx.listener(|this, _, window, cx| this.go_forward(&GoForward, window, cx))),
            ))
            .child(
                nav("browser-reload", if state.loading { IconName::X } else { IconName::RefreshCw }, has_page)
                    .when(has_page, |button| {
                        button.on_click(cx.listener(|this, _, window, cx| this.reload(&Reload, window, cx)))
                    }),
            )
            .child(
                div()
                    .id("browser-address")
                    .relative()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1p5()
                    .h_6()
                    .mx_1()
                    .px_2()
                    .rounded(rems(0.375))
                    .border_1()
                    .border_color(address_border)
                    .bg(theme.secondary)
                    .text_size(rems(0.75))
                    .line_height(rems(1.))
                    .text_color(theme.foreground)
                    .cursor_text()
                    .child(
                        canvas(move |bounds, _, _| address_bounds.set(bounds), |_, _, _, _| {})
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                    )
                    .child(ui::icon(security_icon).size_3().flex_shrink_0().text_color(security_color))
                    .when_some(warning, |bar, warning| {
                        bar.child(
                            div()
                                .flex_shrink_0()
                                .pr_1p5()
                                .border_r_1()
                                .border_color(theme.border)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(security_color)
                                .child(warning),
                        )
                    })
                    .child(div().flex_1().min_w_0().child(self.address.clone())),
            )
            .child(
                div()
                    .relative()
                    .child(
                        ui::small_icon_button("browser-console", IconName::Bug, theme)
                            .when(self.console_open, |button| button.bg(theme.secondary))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_console(cx))),
                    )
                    .when(errors > 0, |button| {
                        let count = if errors > 99 { "99+".to_string() } else { errors.to_string() };
                        button.child(
                            div()
                                .absolute()
                                .top(rems(-0.125))
                                .right(rems(-0.125))
                                .flex()
                                .items_center()
                                .justify_center()
                                .min_w(rems(0.875))
                                .h(rems(0.875))
                                .px(rems(0.1875))
                                .rounded_full()
                                .bg(theme.tints.destructive.base)
                                .text_color(theme.tints.destructive.foreground)
                                .text_size(rems(0.5625))
                                .line_height(rems(0.875))
                                .font_weight(FontWeight::BOLD)
                                .child(count),
                        )
                    }),
            )
            .child(
                ui::small_icon_button("browser-more", IconName::Ellipsis, theme)
                    .child(self.anchors.track("more"))
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.more_menu(this.anchors.below("more", ui::Align::End, event.position()), cx);
                    })),
            )
            .when_some(progress, |toolbar, progress| {
                toolbar.child(
                    div()
                        .absolute()
                        .left_0()
                        .bottom(px(-1.))
                        .h(px(2.))
                        .w(relative(progress))
                        .bg(theme.tints.blue.base),
                )
            })
    }

    fn render_find(&self, find: &FindBar, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "macos")]
        let can_find = self.web.as_ref().is_some_and(|web| web.can_find());
        #[cfg(not(target_os = "macos"))]
        let can_find = false;
        let muted = theme.muted_foreground;
        let missing = find.found == Some(false) && !find.query.is_empty();

        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .h_8()
            .pl_2p5()
            .pr_1p5()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.card_header)
            .text_size(rems(0.75))
            .line_height(rems(1.))
            .child(ui::icon(IconName::Search).size_3().flex_shrink_0().text_color(muted))
            .child(if can_find {
                div().flex_1().min_w_0().ml_1().child(find.input.clone())
            } else {
                div().flex_1().min_w_0().ml_1().truncate().text_color(muted).child(t("Finding text needs macOS 13 or later."))
            })
            .when(missing, |bar| {
                bar.child(div().flex_shrink_0().px_1().text_color(theme.tints.destructive.base).child(t("No matches")))
            })
            .when(can_find, |bar| {
                bar.child(
                    ui::small_icon_button("browser-find-previous", IconName::ChevronUp, theme)
                        .on_click(cx.listener(|this, _, _, cx| this.find_next(true, cx))),
                )
                .child(
                    ui::small_icon_button("browser-find-next", IconName::ChevronDown, theme)
                        .on_click(cx.listener(|this, _, _, cx| this.find_next(false, cx))),
                )
            })
            .child(
                ui::small_icon_button("browser-find-close", IconName::X, theme)
                    .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
            )
    }

    fn render_device_bar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let device = self.settings.device.as_deref().and_then(devices::find);
        let kind = device.map(|device| device.kind);
        let (active_bg, fg, muted) = (theme.secondary, theme.foreground, theme.muted_foreground);
        let quick = |id: &'static str, icon: IconName, on: bool| {
            ui::small_icon_button(id, icon, theme).when(on, |button| button.bg(active_bg))
        };
        let zoom = self.settings.page_zoom.unwrap_or(100);
        let size_label = self.layout.get().map(|layout| {
            let size = format!("{} × {}", layout.css_width as u32, layout.css_height as u32);
            if zoom != 100 { format!("{size}  ·  {zoom}%") } else { size }
        });
        let rotatable = device.is_some_and(|device| device.kind != Kind::Computer);

        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_0p5()
            .h_8()
            .px_1p5()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.card_header)
            .text_size(rems(0.75))
            .line_height(rems(1.))
            .text_color(muted)
            .child(
                quick("browser-responsive", IconName::Maximize, device.is_none())
                    .on_click(cx.listener(|this, _, _, cx| this.set_device(None, cx))),
            )
            .children(QUICK.into_iter().map(|(id, quick_kind)| {
                quick(
                    match quick_kind {
                        Kind::Phone => "browser-phone",
                        Kind::Tablet => "browser-tablet",
                        Kind::Computer => "browser-computer",
                    },
                    quick_kind.icon(),
                    kind == Some(quick_kind),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    // The same kind stays on the device picked from the
                    // menu; another kind starts at its usual one.
                    let current = this.settings.device.as_deref().and_then(devices::find);
                    if current.map(|device| device.kind) != Some(quick_kind) {
                        this.set_device(Some(id), cx);
                    }
                }))
            }))
            .child(div().w_px().h(rems(1.)).mx_1().bg(theme.border))
            .child(
                div()
                    .id("browser-device")
                    .flex()
                    .flex_shrink()
                    .min_w_0()
                    .items_center()
                    .gap_1()
                    .h_6()
                    .px_2()
                    .rounded(rems(0.375))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(fg)
                    .hover(move |style| style.bg(active_bg))
                    .child(div().truncate().child(device.map_or(t("Responsive"), |device| device.name)))
                    .child(ui::icon(IconName::ChevronDown).size_3().text_color(muted))
                    .child(self.anchors.track("device"))
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.device_menu(this.anchors.below("device", ui::Align::Start, event.position()), cx);
                    })),
            )
            .when(rotatable, |bar| {
                bar.child(
                    quick("browser-rotate", IconName::RotateCcw, self.settings.landscape)
                        .on_click(cx.listener(|this, _, _, cx| this.rotate(cx))),
                )
            })
            .child(div().flex_1())
            .when_some(self.notice.clone(), |bar, notice| {
                bar.child(div().flex_shrink_0().pr_2().font_weight(FontWeight::MEDIUM).text_color(fg).child(notice))
            })
            .children(size_label.map(|label| div().flex_shrink_0().pr_1().child(SharedString::from(label))))
    }

    fn render_page(&mut self, recent: Vec<Visit>, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let fixed = devices::fixed_size(self.settings.device.as_deref(), self.settings.landscape);
        let device_mode = fixed.is_some();
        let layout = self.layout.clone();
        let viewport = self.viewport.clone();
        #[cfg(target_os = "macos")]
        let active = self.active.clone();
        let snapshot = self.snapshot.clone();
        let retired = std::mem::take(&mut self.retired);
        let entity = cx.entity_id();
        let (frame_color, shadow) = (theme.border, gpui::black().opacity(0.18));
        let empty = self.settings.url.is_empty();
        let failed = self.failed.clone();
        #[cfg(target_os = "macos")]
        let web = self.web.clone().filter(|_| !empty && failed.is_none());
        let supported = self.has_page();
        let show_page = supported && !empty && failed.is_none();
        let page_zoom = self.page_zoom();
        let scroll = self.scroll;
        // The console sits under the page: its bottom isn't the window's.
        #[cfg(target_os = "macos")]
        let console_open = self.console_open;
        // Scrollbars (from the last layout): their gutters keep them clear
        // of the page, which covers anything drawn under it.
        let last = self.layout.get().filter(|_| device_mode);
        let (vertical, horizontal) = last.map_or((false, false), |l| (l.max_scroll.1 > 0., l.max_scroll.0 > 0.));
        let gutter = |on: bool| if on { rems(GUTTER) } else { rems(0.) };

        div()
            .id("browser-page")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .bg(if device_mode { theme.muted } else { theme.background })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !this.settings.url.is_empty() {
                        window.focus(&this.focus_handle);
                        cx.notify();
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(Self::scroll_area))
            .on_drag_move(cx.listener(|this, event: &DragMoveEvent<ScrollThumb>, _, cx| this.drag_thumb(event, cx)))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right(gutter(vertical))
                    .bottom(gutter(horizontal))
                    .overflow_hidden()
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, cx| {
                                for image in retired {
                                    window.drop_image(image).ok();
                                }
                                // Screen px per layout px while windows are
                                // laid out zoomed, times the page zoom.
                                let zoom = f32::from(window.rem_size()) / 16. * page_zoom;
                                let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                                viewport.set((width, height));
                                let laid_out = devices::layout(fixed, width, height, zoom, scroll);
                                if layout.get() != Some(laid_out) {
                                    layout.set(Some(laid_out));
                                    // The size readout and the scrollbars.
                                    cx.notify(entity);
                                }
                                if !show_page {
                                    return;
                                }
                                let page = Bounds::new(
                                    point(bounds.origin.x + px(laid_out.x), bounds.origin.y + px(laid_out.y)),
                                    size(px(laid_out.width), px(laid_out.height)),
                                );
                                if device_mode {
                                    let ring = page.dilate(px(1.));
                                    let unit = f32::from(window.rem_size()) / 16.;
                                    window.paint_quad(
                                        fill(page.dilate(px(6. * unit)), shadow.opacity(0.08)).corner_radii(px(10. * unit)),
                                    );
                                    window.paint_quad(outline(ring, frame_color, gpui::BorderStyle::Solid));
                                }
                                window.paint_quad(fill(page, gpui::white()));
                                if let Some(snapshot) = snapshot {
                                    window.paint_image(page, Corners::default(), snapshot, 0, false).ok();
                                }
                                #[cfg(target_os = "macos")]
                                if let Some(web) = web {
                                    // The window's bottom corners, which a page
                                    // filling the window reaches.
                                    let unit = f32::from(window.rem_size()) / 16.;
                                    let radius =
                                        if device_mode || console_open { 0. } else { (14. * unit - 1.).max(0.) };
                                    let paint = layers::PagePaint {
                                        bounds: page,
                                        zoom: laid_out.zoom,
                                        corner_radius: radius,
                                        interactive: active.get(),
                                        at_window_edges: !device_mode,
                                    };
                                    layers::page(web, paint, window, cx);
                                }
                            },
                        )
                        .size_full(),
                    ),
            )
            .children(last.filter(|_| show_page).map(|layout| self.render_scrollbars(layout, theme, cx)))
            .when(supported && empty, |area| area.child(self.render_start(recent, theme, cx)))
            .when_some(failed.filter(|_| supported), |area, (url, message)| {
                area.child(self.render_failed(&url, &message, theme, cx))
            })
            .when(!supported, |area| area.child(self.render_unsupported(theme, cx)))
    }

    /// The scrollbars around a device too big for the window, in their
    /// gutters.
    fn render_scrollbars(&self, layout: devices::Layout, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let viewport = self.viewport.get();
        let browser = cx.entity_id();
        let (track_color, thumb_color) = (theme.muted, theme.muted_foreground.opacity(0.45));
        let bar = |vertical: bool| {
            let (start, length, _) = Self::thumb(&layout, viewport, vertical)?;
            let thumb = div()
                .id(if vertical { "browser-scroll-y" } else { "browser-scroll-x" })
                .absolute()
                .rounded_full()
                .bg(thumb_color)
                .hover(move |style| style.bg(thumb_color.opacity(0.8)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, _, _| {
                        this.thumb_drag = Some((event.position, this.scroll));
                    }),
                )
                .on_drag(ScrollThumb { browser, vertical }, |_, _, _, cx| cx.new(|_| NoPreview));
            let thumb = if vertical {
                thumb.top(px(start)).h(px(length)).left(rems(0.1875)).right(rems(0.1875))
            } else {
                thumb.left(px(start)).w(px(length)).top(rems(0.1875)).bottom(rems(0.1875))
            };
            let track = div().absolute().bg(track_color).child(thumb);
            Some(if vertical {
                track.top_0().right_0().w(rems(GUTTER)).h(px(viewport.1))
            } else {
                track.left_0().bottom_0().h(rems(GUTTER)).w(px(viewport.0))
            })
        };
        div().children(bar(true)).children(bar(false))
    }

    fn render_console(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let (count, errors) = (self.console.len(), self.console.errors());
        let muted = theme.muted_foreground;
        let tints = theme.tints;

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .h(rems(10.))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .h_7()
                    .pl_2p5()
                    .pr_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.card_header)
                    .text_size(rems(0.75))
                    .line_height(rems(1.))
                    .text_color(muted)
                    .child(ui::icon(IconName::Bug).size_3().text_color(muted))
                    .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.foreground).child(t("Console")))
                    .child(tn(count, "{} message", "{} messages"))
                    .when(errors > 0, |header| {
                        header.child(div().text_color(tints.destructive.base).child(tn(errors, "{} error", "{} errors")))
                    })
                    .child(div().flex_1())
                    .child(ui::small_icon_button("browser-console-clear", IconName::Trash, theme).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.console.clear();
                            cx.notify();
                        },
                    )))
                    .child(
                        ui::small_icon_button("browser-console-close", IconName::X, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_console(cx))),
                    ),
            )
            .child(
                div()
                    .id("browser-console-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.console_scroll)
                    .text_size(rems(0.6875))
                    .line_height(rems(1.))
                    .when(count == 0, |list| {
                        list.child(div().px_2p5().py_2().text_color(muted).child(t("Nothing logged yet.")))
                    })
                    .children(self.console.messages().map(|message| {
                        let (tint, marker) = match message.level {
                            Level::Error => (Some(tints.destructive), Some(IconName::TriangleAlert)),
                            Level::Warn => (Some(tints.amber), Some(IconName::TriangleAlert)),
                            Level::Info | Level::Log | Level::Debug => (None, None),
                        };
                        let text_color = match (tint, message.level) {
                            (Some(tint), _) => tint.soft_foreground,
                            (None, Level::Debug) => muted,
                            (None, _) => theme.foreground,
                        };
                        div()
                            .flex()
                            .flex_shrink_0()
                            .gap_1p5()
                            .px_2p5()
                            .py(rems(0.1875))
                            .border_b_1()
                            .border_color(theme.border.opacity(0.6))
                            .when_some(tint, |row, tint| row.bg(tint.soft))
                            .child(
                                div().flex_shrink_0().w(rems(0.75)).pt(rems(0.125)).children(
                                    marker.map(|icon| ui::icon(icon).size(rems(0.6875)).text_color(tint.map_or(muted, |t| t.base))),
                                ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_family(FONT_MONO)
                                    .text_color(text_color)
                                    .child(message.text.clone()),
                            )
                    })),
            )
    }

    /// The address bar's suggestions, under it.
    fn render_suggestions(&self, theme: &Theme, cx: &Context<Self>) -> Option<AnyElement> {
        if self.suggestions.is_empty() || !self.address_focused {
            return None;
        }
        let (root, field) = (self.root_bounds.get(), self.address_bounds.get());
        if field.size.width <= px(0.) {
            return None;
        }
        let (hover, muted) = (theme.secondary, theme.muted_foreground);
        Some(
            div()
                .id("browser-suggestions")
                .absolute()
                .left(field.origin.x - root.origin.x)
                .top(field.origin.y - root.origin.y + field.size.height + px(4.))
                .w(field.size.width)
                .flex()
                .flex_col()
                .p_1()
                .rounded(rems(0.5))
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .text_color(theme.popover_foreground)
                .text_size(rems(0.75))
                .line_height(rems(1.))
                .shadow_md()
                .occlude()
                // The page is a native view: without this it would cover
                // the list.
                .child(layers::occluder())
                .children(self.suggestions.iter().enumerate().map(|(ix, visit)| {
                    let url = visit.url.clone();
                    let title = if visit.title.trim().is_empty() { address::display(&visit.url) } else { visit.title.clone() };
                    div()
                        .id(ElementId::named_usize("browser-suggestion", ix))
                        .flex()
                        .items_center()
                        .gap_2()
                        .h_7()
                        .px_2()
                        .rounded(rems(0.375))
                        .when(self.suggestion == Some(ix), |row| row.bg(hover))
                        .hover(move |style| style.bg(hover))
                        .child(ui::icon(IconName::History).size_3().flex_shrink_0().text_color(muted))
                        .child(div().flex_shrink().min_w_0().truncate().child(title))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(muted)
                                .child(address::display(&visit.url)),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.navigate(&url, window, cx);
                            }),
                        )
                }))
                .into_any_element(),
        )
    }

    fn panel(theme: &Theme) -> gpui::Div {
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
            .px_6()
            .bg(theme.background)
            .text_size(rems(0.8125))
            .line_height(rems(1.25))
            .text_color(theme.muted_foreground)
    }

    fn title(text: impl Into<SharedString>, theme: &Theme) -> impl IntoElement {
        div()
            .text_size(rems(1.))
            .line_height(rems(1.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.foreground)
            .child(text.into())
    }

    fn render_start(&self, recent: Vec<Visit>, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let (hover, muted) = (theme.secondary, theme.muted_foreground);
        Self::panel(theme)
            .child(ui::icon(IconName::Globe).size_8().text_color(muted))
            .child(Self::title(t("Open a page"), theme))
            .child(div().text_center().child(t("Type an address above, or a dev server's port, like 3000.")))
            .child(div().mt_2().flex().flex_wrap().justify_center().gap_2().children(DEV_SERVERS.iter().enumerate().map(
                |(ix, server)| {
                    let server = *server;
                    ui::compact_button(ElementId::named_usize("browser-dev-server", ix), Some(IconName::Play), server, false, theme)
                        .on_click(cx.listener(move |this, _, window, cx| this.navigate(server, window, cx)))
                },
            )))
            .when(!recent.is_empty(), |panel| {
                panel.child(
                    div()
                        .mt_4()
                        .w_full()
                        .max_w(rems(24.))
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            div()
                                .px_2()
                                .pb_1()
                                .text_size(rems(0.6875))
                                .font_weight(FontWeight::MEDIUM)
                                .child(t("Recently visited")),
                        )
                        .children(recent.into_iter().enumerate().map(|(ix, visit)| {
                            let url = visit.url.clone();
                            let title =
                                if visit.title.trim().is_empty() { address::display(&visit.url) } else { visit.title.clone() };
                            div()
                                .id(ElementId::named_usize("browser-recent", ix))
                                .flex()
                                .items_center()
                                .gap_2()
                                .h_7()
                                .px_2()
                                .rounded(rems(0.375))
                                .text_size(rems(0.75))
                                .line_height(rems(1.))
                                .hover(move |style| style.bg(hover))
                                .child(ui::icon(IconName::History).size_3().flex_shrink_0().text_color(muted))
                                .child(div().flex_shrink().min_w_0().truncate().text_color(theme.foreground).child(title))
                                .child(div().flex_1().min_w_0().truncate().child(address::display(&visit.url)))
                                .on_click(cx.listener(move |this, _, window, cx| this.navigate(&url, window, cx)))
                        })),
                )
            })
    }

    fn render_failed(&self, url: &str, message: &str, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        Self::panel(theme)
            .child(Self::title(t("Can't open this page"), theme))
            .child(div().text_color(theme.foreground).truncate().max_w_full().child(address::display(url)))
            .child(div().text_center().child(message.to_string()))
            .child(
                div().mt_2().child(
                    ui::compact_button("browser-try-again", Some(IconName::RefreshCw), t("Try again"), true, theme)
                        .on_click(cx.listener(|this, _, window, cx| this.reload(&Reload, window, cx))),
                ),
            )
    }

    fn render_unsupported(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let url = (!self.settings.url.is_empty()).then(|| self.settings.url.clone());
        Self::panel(theme)
            .child(ui::icon(IconName::Globe).size_8().text_color(theme.muted_foreground))
            .child(Self::title(t("Pages show on macOS for now"), theme))
            .child(div().text_center().child(t("Open this one in your browser instead.")))
            .when_some(url, |panel, url| {
                panel.child(
                    div().mt_2().child(
                        ui::compact_button("browser-open-outside", Some(IconName::ExternalLink), t("Open in browser"), true, theme)
                            .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                    ),
                )
            })
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window = Some(window.window_handle());
        // The native page takes the keyboard while the page is focused, and
        // gives it back when it isn't.
        let page_focused = self.focus_handle.is_focused(window);
        if page_focused != self.page_focused {
            self.page_focused = page_focused;
            #[cfg(target_os = "macos")]
            if let Some(web) = &self.web {
                if page_focused { web.focus() } else { web.blur() }
            }
        }
        let address_focused = self.address.focus_handle(cx).is_focused(window);
        if address_focused && !self.address_focused {
            // Clicking into the address bar selects it, as in Safari.
            self.address.update(cx, |input, cx| input.select_all_text(cx));
        }
        self.address_focused = address_focused;
        self.update_suggestions(cx);
        let recent = if self.settings.url.is_empty() && self.has_page() {
            History::get(cx).recent(RECENT)
        } else {
            Vec::new()
        };
        let theme = Theme::global(cx).clone();
        let root_bounds = self.root_bounds.clone();

        div()
            .id("browser")
            .key_context(KEY_CONTEXT)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_address))
            .on_action(cx.listener(Self::reload))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::find_in_page))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .child(
                canvas(move |bounds, _, _| root_bounds.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
            .child(self.render_toolbar(&theme, cx))
            .children(self.find.as_ref().map(|find| self.render_find(find, &theme, cx)))
            .child(self.render_device_bar(&theme, cx))
            .child(self.render_page(recent, &theme, cx))
            .when(self.console_open, |browser| browser.child(self.render_console(&theme, cx)))
            .children(self.render_suggestions(&theme, cx))
    }
}
