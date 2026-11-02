//! Web pages for Browser windows: WebKit's `WKWebView`, a native view laid
//! over GPUI's in the same window (GPUI draws everything with Metal, so it
//! can't host one inside its own drawing).
//!
//! The page sits in a clipping view, a sibling just above GPUI's view, at
//! the place the Browser window paints for it each frame (see
//! `windows/browser/layers.rs`, which also hides it when something GPUI
//! draws is in front of it). Native views take clicks and typing before
//! GPUI sees them, so an event monitor tells the Browser when its page is
//! clicked (to make its window the active one) and hands the keyboard back
//! to GPUI when you click anything else.

use std::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    rc::{Rc, Weak},
    sync::Arc,
};

use block2::RcBlock;
use futures::channel::mpsc::UnboundedSender;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel},
    sel,
};
use objc2_app_kit::{
    NSApplication, NSBitmapImageFileType, NSBitmapImageRep, NSDeviceRGBColorSpace, NSEvent, NSEventMask,
    NSEventModifierFlags, NSEventType, NSGraphicsContext, NSImage, NSResponder, NSView, NSWindow, NSWindowOrderingMode,
};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
use objc2_web_kit::{
    WKFindConfiguration, WKFindResult, WKNavigation, WKNavigationAction, WKNavigationDelegate, WKScriptMessage,
    WKScriptMessageHandler, WKSnapshotConfiguration, WKUIDelegate, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration, WKWindowFeatures,
};

use crate::windows::browser::console;

/// What a page tells its Browser window.
pub enum WebEvent {
    /// The page was clicked: its window should be the active one.
    Clicked,
    /// A new page started showing (a link, back, a reload): what the last
    /// one logged is gone.
    Committed,
    /// The page finished loading.
    Finished,
    /// A page didn't load (no server there, no network, …).
    Failed { url: String, message: String },
    /// How the page looked, to show while something covers it.
    Snapshot(Arc<gpui::RenderImage>),
    /// A screenshot of the page, as PNG (see [`WebView::screenshot`]).
    Screenshot(Vec<u8>),
    /// The page logged something, or an error went uncaught.
    Console(console::Message),
    /// Whether the last search found the text (see [`WebView::find`]).
    Found(bool),
    /// A Browser shortcut typed while the page had the keyboard.
    Shortcut(Shortcut),
}

/// Shortcuts the Browser window handles even while its page has the
/// keyboard (which would otherwise swallow them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    /// ⌘F
    Find,
    /// ⌘L
    FocusAddress,
    /// ⌘R
    Reload,
    /// ⌘[
    Back,
    /// ⌘]
    Forward,
}

/// Where the page is, read when the Browser asks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageState {
    pub url: String,
    pub title: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub loading: bool,
    /// 0 to 1 while loading.
    pub progress: f64,
}

/// Where to show the page, in window coordinates (GPUI's: logical px from
/// the top left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// The part of the page that's visible (the page, clipped to the
    /// canvas).
    pub visible: gpui::Bounds<gpui::Pixels>,
    /// The whole page.
    pub page: gpui::Bounds<gpui::Pixels>,
    /// Screen px per CSS px.
    pub zoom: f32,
    /// Rounds the bottom corners (the window's own), when they show.
    pub corner_radius: f32,
    /// Takes the mouse: only in the window you're working in. Elsewhere
    /// clicks, scrolling and swipes go to the canvas, as over any window.
    pub interactive: bool,
    /// The page's left, right and bottom edges are its window's, where the
    /// window's resize handles are: the page leaves the mouse to those.
    pub at_window_edges: bool,
}

/// How far a canvas window's resize handles reach in from its edges and
/// corners, in px (see `resize_handle` in `canvas/mod.rs`).
const HANDLE_EDGE: f64 = 4.;
const HANDLE_CORNER: f64 = 7.;

/// What the clip view lets through to the page, in window coordinates.
#[derive(Default)]
struct ClipIvars {
    interactive: Cell<bool>,
    at_window_edges: Cell<bool>,
    /// The page (GPUI coordinates) and the window's height then, to turn
    /// AppKit points into GPUI ones.
    page: Cell<NSRect>,
    height: Cell<f64>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond its own
    // methods, and PageClip has no Drop of its own.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "XHarnessPageClip"]
    #[ivars = ClipIvars]
    struct PageClip;

    impl PageClip {
        /// Mouse events go to the view this returns: none (so GPUI's view,
        /// below, gets them) where the page isn't taking the mouse.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            if !self.takes_mouse_at(point) {
                return std::ptr::null_mut();
            }
            // SAFETY: NSView's own hit testing.
            unsafe { msg_send![super(self), hitTest: point] }
        }
    }
);

impl PageClip {
    fn new(main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(ClipIvars::default());
        // SAFETY: NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }

    /// `point` is in the superview's coordinates (the window's content).
    fn takes_mouse_at(&self, point: NSPoint) -> bool {
        let ivars = self.ivars();
        if !ivars.interactive.get() {
            return false;
        }
        if !ivars.at_window_edges.get() {
            return true;
        }
        let (x, y) = (point.x, ivars.height.get() - point.y);
        let page = ivars.page.get();
        let (left, right, bottom) = (page.origin.x, page.origin.x + page.size.width, page.origin.y + page.size.height);
        let near_side = x < left + HANDLE_EDGE || x > right - HANDLE_EDGE;
        let near_bottom = y > bottom - HANDLE_EDGE;
        let in_corner = y > bottom - HANDLE_CORNER && (x < left + HANDLE_CORNER || x > right - HANDLE_CORNER);
        !(near_side || near_bottom || in_corner)
    }
}

struct DelegateIvars {
    events: UnboundedSender<WebEvent>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Delegate has no
    // Drop of its own (its ivars drop normally).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "XHarnessWebDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl WKNavigationDelegate for Delegate {
        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, _web: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.ivars().events.unbounded_send(WebEvent::Committed).ok();
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _web: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.ivars().events.unbounded_send(WebEvent::Finished).ok();
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(&self, _web: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            self.failed(error);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, _web: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            self.failed(error);
        }
    }

    unsafe impl WKUIDelegate for Delegate {
        /// Links that open a new window (`target="_blank"`, `window.open`)
        /// open in this one: a Browser window is one page. Returns no new
        /// view (nil).
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(
            &self,
            web: &WKWebView,
            _configuration: &WKWebViewConfiguration,
            action: &WKNavigationAction,
            _features: &WKWindowFeatures,
        ) -> *mut WKWebView {
            // SAFETY: loading a request the page made, in the view that made it.
            unsafe { web.loadRequest(&action.request()) };
            std::ptr::null_mut()
        }
    }
);

impl Delegate {
    fn new(events: UnboundedSender<WebEvent>, main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(DelegateIvars { events });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }

    fn failed(&self, error: &NSError) {
        // Cancelled (another navigation started, or a download), and
        // "frame load interrupted" (a file WebKit hands off): not failures.
        let code = error.code();
        let domain = error.domain().to_string();
        if (domain == "NSURLErrorDomain" && code == -999) || (domain == "WebKitErrorDomain" && code == 102) {
            return;
        }
        // The address that failed: a string, or (from WebKit) a URL.
        let info = error.userInfo();
        let url = info
            .objectForKey(&NSString::from_str("NSErrorFailingURLStringKey"))
            .and_then(|value| value.downcast::<NSString>().ok())
            .map(|url| url.to_string())
            .or_else(|| {
                info.objectForKey(&NSString::from_str("NSErrorFailingURLKey"))
                    .and_then(|value| value.downcast::<NSURL>().ok())
                    .and_then(|url| url.absoluteString())
                    .map(|url| url.to_string())
            })
            .unwrap_or_default();
        let message = error.localizedDescription().to_string();
        self.ivars().events.unbounded_send(WebEvent::Failed { url, message }).ok();
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and ConsoleHandler
    // has no Drop of its own (its ivars drop normally).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "XHarnessConsoleHandler"]
    #[ivars = DelegateIvars]
    struct ConsoleHandler;

    unsafe impl NSObjectProtocol for ConsoleHandler {}

    unsafe impl WKScriptMessageHandler for ConsoleHandler {
        /// A message from the console script (see `browser/console.rs`):
        /// JSON, as a string.
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // SAFETY: reading the message's body on the main thread.
            let body = unsafe { message.body() };
            let Ok(json) = body.downcast::<NSString>() else {
                return;
            };
            if let Some(message) = console::parse(&json.to_string()) {
                self.ivars().events.unbounded_send(WebEvent::Console(message)).ok();
            }
        }
    }
);

impl ConsoleHandler {
    fn new(events: UnboundedSender<WebEvent>, main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(DelegateIvars { events });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

/// One page.
pub struct WebView {
    web: Retained<WKWebView>,
    /// Clips the page to the part that's visible, and decides where it
    /// takes the mouse.
    clip: Retained<PageClip>,
    _delegate: Retained<Delegate>,
    /// Kept by the page's content controller too (removed on drop, which
    /// would otherwise keep both alive).
    _console: Retained<ConsoleHandler>,
    events: UnboundedSender<WebEvent>,
    /// GPUI's view in the window the page is in.
    host: RefCell<Option<Retained<NSView>>>,
    /// Where it's shown, and the window's height then (AppKit counts from
    /// the bottom, so a taller window moves it).
    shown: Cell<Option<(Placement, f64)>>,
    /// The page should have the keyboard: taken when it shows (hiding a
    /// view gives the keyboard up, and a page can be asked before it first
    /// shows).
    wants_keyboard: Cell<bool>,
    /// The window's pixels per point, for snapshots.
    scale: Cell<f32>,
}

thread_local! {
    /// Every page, for the event monitor.
    static PAGES: RefCell<Vec<Weak<WebView>>> = const { RefCell::new(Vec::new()) };
    static MONITORING: Cell<bool> = const { Cell::new(false) };
}

impl WebView {
    /// A new page, showing nothing yet. Must be called on the main thread.
    pub fn new(events: UnboundedSender<WebEvent>) -> Option<Rc<Self>> {
        let main_thread = MainThreadMarker::new()?;
        let console_handler = ConsoleHandler::new(events.clone(), main_thread);
        // SAFETY: WebKit's own setup, on the main thread.
        let web = unsafe {
            let configuration = WKWebViewConfiguration::new(main_thread);
            // The console script runs in every page before the page's own,
            // and posts to the handler (see `browser/console.rs`).
            let content = configuration.userContentController();
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(main_thread),
                &NSString::from_str(console::SCRIPT),
                WKUserScriptInjectionTime::AtDocumentStart,
                true,
            );
            content.addUserScript(&script);
            content.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*console_handler),
                &NSString::from_str(console::HANDLER),
            );
            // "Inspect Element" in the page's menu opens Web Inspector. A
            // private preference: set only while WebKit still has it (an
            // unknown key would raise).
            let preferences = configuration.preferences();
            let has_it: bool = msg_send![&*preferences, respondsToSelector: sel!(_setDeveloperExtrasEnabled:)];
            if has_it {
                let on = NSNumber::new_bool(true);
                let _: () = msg_send![&*preferences, setValue: &*on, forKey: &*NSString::from_str("developerExtrasEnabled")];
            }
            let web = WKWebView::initWithFrame_configuration(WKWebView::alloc(main_thread), NSRect::ZERO, &configuration);
            // And Safari's Develop menu can inspect it too.
            web.setInspectable(true);
            web.setAllowsBackForwardNavigationGestures(true);
            web
        };
        let delegate = Delegate::new(events.clone(), main_thread);
        // SAFETY: the delegate outlives the view's use of it (both are
        // dropped together, the view first).
        unsafe {
            web.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            web.setUIDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        }
        let clip = PageClip::new(main_thread);
        // Keeps its distance from the top while the window is resized,
        // until the next frame places it exactly.
        clip.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin);
        clip.setWantsLayer(true);
        clip.setClipsToBounds(true);
        clip.setHidden(true);
        clip.addSubview(&web);

        let page = Rc::new(Self {
            web,
            clip,
            _delegate: delegate,
            _console: console_handler,
            events,
            host: RefCell::new(None),
            shown: Cell::new(None),
            wants_keyboard: Cell::new(false),
            scale: Cell::new(2.),
        });
        PAGES.with_borrow_mut(|pages| {
            pages.retain(|page| page.strong_count() > 0);
            pages.push(Rc::downgrade(&page));
        });
        install_monitor(main_thread);
        Some(page)
    }

    /// Puts the page in `window` (GPUI's), above GPUI's own view, if it
    /// isn't there already.
    pub fn attach(&self, window: &gpui::Window) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        self.scale.set(window.scale_factor());
        let Ok(handle) = HasWindowHandle::window_handle(window) else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };
        let pointer = handle.ns_view.as_ptr().cast::<NSView>();
        if self.host.borrow().as_ref().is_some_and(|host| std::ptr::eq(&**host, pointer)) {
            return;
        }
        // SAFETY: GPUI's view for this window, alive while the window is.
        let Some(host) = (unsafe { Retained::retain(pointer) }) else {
            return;
        };
        // SAFETY: reading GPUI's view's superview (the window's content view).
        let Some(parent) = (unsafe { host.superview() }) else {
            return;
        };
        self.clip.removeFromSuperview();
        parent.addSubview_positioned_relativeTo(&self.clip, NSWindowOrderingMode::Above, Some(&host));
        self.shown.set(None);
        self.clip.setHidden(true);
        *self.host.borrow_mut() = Some(host);
    }

    /// Shows the page at `placement`, or hides it (`None`).
    pub fn place(&self, placement: Option<Placement>) {
        let Some(placement) = placement else {
            if self.shown.get().is_none() {
                return;
            }
            // Hiding a view that has the keyboard hands it to the window,
            // where GPUI never gets it back: hand it to GPUI first. It
            // comes back to the page when it shows (`wants_keyboard`).
            self.release_keyboard();
            self.clip.setHidden(true);
            self.shown.set(None);
            return;
        };
        // SAFETY: as above, for the clip view.
        let Some(height) = (unsafe { self.clip.superview() }).map(|parent| parent.bounds().size.height) else {
            return;
        };
        if self.shown.get() == Some((placement, height)) {
            return;
        }
        let (visible, page) = (rect(placement.visible), rect(placement.page));
        let ivars = self.clip.ivars();
        ivars.interactive.set(placement.interactive);
        ivars.at_window_edges.set(placement.at_window_edges);
        ivars.page.set(page);
        ivars.height.set(height);
        // AppKit's y goes up from the bottom; GPUI's down from the top.
        self.clip.setFrame(NSRect::new(
            NSPoint::new(visible.origin.x, height - visible.origin.y - visible.size.height),
            visible.size,
        ));
        self.web.setFrame(NSRect::new(
            NSPoint::new(
                page.origin.x - visible.origin.x,
                (visible.origin.y + visible.size.height) - (page.origin.y + page.size.height),
            ),
            page.size,
        ));
        // SAFETY: plain WebKit property calls on the main thread.
        unsafe {
            if (self.web.pageZoom() - placement.zoom as f64).abs() > 1e-4 {
                self.web.setPageZoom(placement.zoom as f64);
            }
        }
        // The window's rounded bottom corners, when the page's bottom edge
        // is the window's and in view.
        let bottom_shows = (visible.origin.y + visible.size.height - (page.origin.y + page.size.height)).abs() < 0.5;
        let radius = if bottom_shows { placement.corner_radius as f64 } else { 0. };
        if let Some(layer) = self.clip.layer() {
            let layer: &AnyObject = &layer;
            // SAFETY: CALayer properties: kCALayerMinXMinYCorner |
            // kCALayerMaxXMinYCorner, the bottom ones (the layer isn't
            // flipped).
            unsafe {
                let _: () = msg_send![layer, setCornerRadius: radius];
                let _: () = msg_send![layer, setMaskedCorners: 3usize];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }
        }
        let was_hidden = self.shown.get().is_none();
        self.clip.setHidden(false);
        self.shown.set(Some((placement, height)));
        if was_hidden && self.wants_keyboard.get() {
            self.take_keyboard();
        }
    }

    pub fn is_shown(&self) -> bool {
        self.shown.get().is_some()
    }

    pub fn load(&self, url: &str) {
        let Some(address) = NSURL::URLWithString(&NSString::from_str(url)) else {
            self.events
                .unbounded_send(WebEvent::Failed {
                    url: url.to_string(),
                    message: crate::i18n::t("That isn't a web address.").into(),
                })
                .ok();
            return;
        };
        // SAFETY: WebKit navigation calls on the main thread.
        unsafe {
            if address.isFileURL() {
                // Local pages can read the files next to them (their
                // scripts, styles and images).
                let folder = address.URLByDeletingLastPathComponent().unwrap_or_else(|| address.clone());
                self.web.loadFileURL_allowingReadAccessToURL(&address, &folder);
            } else {
                self.web.loadRequest(&NSURLRequest::requestWithURL(&address));
            }
        }
    }

    pub fn back(&self) {
        // SAFETY: as above.
        unsafe { self.web.goBack() };
    }

    pub fn forward(&self) {
        // SAFETY: as above.
        unsafe { self.web.goForward() };
    }

    pub fn reload(&self) {
        // SAFETY: as above.
        unsafe { self.web.reload() };
    }

    /// Reloads without the cache: every file is asked for again.
    pub fn reload_from_origin(&self) {
        // SAFETY: as above.
        unsafe { self.web.reloadFromOrigin() };
    }

    fn responds_to(object: &AnyObject, selector: Sel) -> bool {
        // SAFETY: NSObject's respondsToSelector:.
        unsafe { msg_send![object, respondsToSelector: selector] }
    }

    /// Whether this macOS can search pages (13 and later).
    pub fn can_find(&self) -> bool {
        Self::responds_to(&self.web, sel!(findString:withConfiguration:completionHandler:))
    }

    /// Finds the next (or previous) `text` in the page, from the current
    /// match, wrapping around; it's selected and scrolled to. Whether it
    /// was found comes as a [`WebEvent::Found`].
    pub fn find(&self, text: &str, backwards: bool) {
        if text.is_empty() || !self.can_find() {
            return;
        }
        let Some(main_thread) = MainThreadMarker::new() else {
            return;
        };
        let events = self.events.clone();
        let done = RcBlock::new(move |result: NonNull<WKFindResult>| {
            // SAFETY: WebKit passes a valid result for the call.
            let found = unsafe { result.as_ref().matchFound() };
            events.unbounded_send(WebEvent::Found(found)).ok();
        });
        // SAFETY: a search in the page, on the main thread; the block is
        // copied by WebKit.
        unsafe {
            let configuration = WKFindConfiguration::new(main_thread);
            configuration.setBackwards(backwards);
            configuration.setWraps(true);
            configuration.setCaseSensitive(false);
            self.web.findString_withConfiguration_completionHandler(&NSString::from_str(text), Some(&configuration), &done);
        }
    }

    /// Whether Web Inspector can be opened from the app (a private WebKit
    /// call; "Inspect Element" in the page's menu works either way).
    pub fn can_inspect(&self) -> bool {
        Self::responds_to(&self.web, sel!(_inspector))
    }

    /// Opens Web Inspector for the page.
    pub fn show_inspector(&self) {
        if !self.can_inspect() {
            return;
        }
        // SAFETY: WebKit's private `_inspector` (checked above), and its
        // `show`, checked on the object it returns.
        unsafe {
            let inspector: *mut AnyObject = msg_send![&*self.web, _inspector];
            if let Some(inspector) = inspector.as_ref()
                && Self::responds_to(inspector, sel!(show))
            {
                let _: () = msg_send![inspector, show];
            }
        }
    }

    /// Takes a screenshot of the page, sent as a [`WebEvent::Screenshot`].
    pub fn screenshot(&self) {
        let Some(main_thread) = MainThreadMarker::new() else {
            return;
        };
        let (events, scale) = (self.events.clone(), self.scale.get());
        let done = RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
            // SAFETY: WebKit passes a valid image, or null.
            if let Some(image) = unsafe { image.as_ref() }
                && let Some(png) = png(image, scale)
            {
                events.unbounded_send(WebEvent::Screenshot(png)).ok();
            }
        });
        // SAFETY: as in `snapshot`.
        unsafe {
            let configuration = WKSnapshotConfiguration::new(main_thread);
            self.web.takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &done);
        }
    }

    pub fn stop(&self) {
        // SAFETY: as above.
        unsafe { self.web.stopLoading() };
    }

    /// What the page says its browser is (`None`: Safari's own). Applies
    /// from the next load.
    pub fn set_user_agent(&self, user_agent: Option<&str>) {
        let user_agent = user_agent.map(NSString::from_str);
        // SAFETY: as above.
        unsafe { self.web.setCustomUserAgent(user_agent.as_deref()) };
    }

    pub fn state(&self) -> PageState {
        // SAFETY: WebKit property reads on the main thread.
        unsafe {
            PageState {
                url: self
                    .web
                    .URL()
                    .and_then(|url| url.absoluteString())
                    .map(|url| url.to_string())
                    .unwrap_or_default(),
                title: self.web.title().map(|title| title.to_string()).unwrap_or_default(),
                can_go_back: self.web.canGoBack(),
                can_go_forward: self.web.canGoForward(),
                loading: self.web.isLoading(),
                progress: self.web.estimatedProgress(),
            }
        }
    }

    /// Gives the page the keyboard (now, or when it shows).
    pub fn focus(&self) {
        self.wants_keyboard.set(true);
        if self.is_shown() {
            self.take_keyboard();
        }
    }

    fn take_keyboard(&self) {
        if self.has_keyboard() {
            return;
        }
        if let Some(window) = self.web.window() {
            let page: &NSResponder = &self.web;
            window.makeFirstResponder(Some(page));
        }
    }

    /// Gives the keyboard back to GPUI, if the page has it.
    pub fn blur(&self) {
        self.wants_keyboard.set(false);
        self.release_keyboard();
    }

    fn release_keyboard(&self) {
        if self.has_keyboard() {
            self.give_keyboard_to_gpui();
        }
    }

    /// Makes GPUI's view the window's first responder.
    fn give_keyboard_to_gpui(&self) {
        if let (Some(window), Some(host)) = (self.web.window(), self.host.borrow().as_ref()) {
            let host: &NSResponder = host;
            window.makeFirstResponder(Some(host));
        }
    }

    /// Whether GPUI's view has the keyboard in the page's window.
    fn gpui_has_keyboard(&self) -> bool {
        let (Some(window), Some(host)) = (self.web.window(), self.host.borrow().clone()) else {
            return true;
        };
        window
            .firstResponder()
            .and_then(|responder| responder.downcast::<NSView>().ok())
            .is_some_and(|view| std::ptr::eq(&*view, &*host))
    }

    /// Whether typing goes to the page.
    pub fn has_keyboard(&self) -> bool {
        let Some(window) = self.web.window() else {
            return false;
        };
        window
            .firstResponder()
            .and_then(|responder| responder.downcast::<NSView>().ok())
            .is_some_and(|view| view.isDescendantOf(&self.web))
    }

    /// Takes a snapshot of the page, sent as a [`WebEvent::Snapshot`].
    pub fn snapshot(&self) {
        let Some(main_thread) = MainThreadMarker::new() else {
            return;
        };
        let (events, scale) = (self.events.clone(), self.scale.get());
        let done = RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
            // SAFETY: WebKit passes a valid image, or null.
            if let Some(image) = unsafe { image.as_ref() }
                && let Some(snapshot) = render_image(image, scale)
            {
                events.unbounded_send(WebEvent::Snapshot(Arc::new(snapshot))).ok();
            }
        });
        // SAFETY: a snapshot of what's on screen; the block is copied by
        // WebKit.
        unsafe {
            let configuration = WKSnapshotConfiguration::new(main_thread);
            configuration.setAfterScreenUpdates(false);
            self.web.takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &done);
        }
    }

    fn is_in(&self, window: &NSWindow) -> bool {
        self.web.window().is_some_and(|own| std::ptr::eq(&*own, window))
    }

    /// The part of the window the page shows in, while it's shown.
    fn contains(&self, window: &NSWindow, point: NSPoint) -> bool {
        if !self.is_shown() || !self.is_in(window) {
            return false;
        }
        let local = self.clip.convertPoint_fromView(point, None);
        let bounds = self.clip.bounds();
        let inside = local.x >= bounds.origin.x
            && local.y >= bounds.origin.y
            && local.x < bounds.origin.x + bounds.size.width
            && local.y < bounds.origin.y + bounds.size.height;
        // The window's point in the content view's coordinates (the same
        // as the window's, which the content view fills).
        inside && self.clip.takes_mouse_at(point)
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.release_keyboard();
        // SAFETY: as above; the content controller lets go of the console
        // handler, which it keeps.
        unsafe {
            self.web.stopLoading();
            self.web
                .configuration()
                .userContentController()
                .removeScriptMessageHandlerForName(&NSString::from_str(console::HANDLER));
        }
        self.clip.removeFromSuperview();
    }
}

fn rect(bounds: gpui::Bounds<gpui::Pixels>) -> NSRect {
    NSRect::new(
        NSPoint::new(f32::from(bounds.origin.x) as f64, f32::from(bounds.origin.y) as f64),
        NSSize::new(f32::from(bounds.size.width) as f64, f32::from(bounds.size.height) as f64),
    )
}

/// `image` drawn into an RGBA bitmap at `scale` pixels per point, and its
/// size in pixels.
fn bitmap(image: &NSImage, scale: f32) -> Option<(Retained<NSBitmapImageRep>, usize, usize)> {
    let size = image.size();
    let (width, height) = ((size.width * scale as f64).round() as usize, (size.height * scale as f64).round() as usize);
    if width == 0 || height == 0 {
        return None;
    }
    // SAFETY: an RGBA bitmap AppKit allocates (no planes passed), drawn
    // into on this thread.
    unsafe {
        let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width as isize,
            height as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            (width * 4) as isize,
            32,
        )?;
        rep.setSize(size);
        let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&context));
        image.drawInRect(NSRect::new(NSPoint::new(0., 0.), size));
        context.flushGraphics();
        NSGraphicsContext::restoreGraphicsState_class();
        Some((rep, width, height))
    }
}

/// `image` drawn at `scale` pixels per point, as GPUI wants it (BGRA).
fn render_image(image: &NSImage, scale: f32) -> Option<gpui::RenderImage> {
    let (rep, width, height) = bitmap(image, scale)?;
    // SAFETY: the bitmap's own pixels (RGBA, `width * 4` bytes a row), read
    // while it's alive.
    let mut pixels = unsafe {
        let data = rep.bitmapData();
        if data.is_null() {
            return None;
        }
        std::slice::from_raw_parts(data, width * height * 4).to_vec()
    };
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
    Some(gpui::RenderImage::new([image::Frame::new(buffer)]))
}

/// `image` drawn at `scale` pixels per point, as a PNG file.
fn png(image: &NSImage, scale: f32) -> Option<Vec<u8>> {
    let (rep, _, _) = bitmap(image, scale)?;
    // SAFETY: AppKit's own encoder, on this thread.
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    Some(data.to_vec())
}

/// Watches clicks and shortcuts before they're delivered (see the module
/// docs). Installed once; it lives as long as the app.
fn install_monitor(_main_thread: MainThreadMarker) {
    if MONITORING.replace(true) {
        return;
    }
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit hands the handler a valid event for the call.
        let event_ref = unsafe { event.as_ref() };
        let Some(main_thread) = MainThreadMarker::new() else {
            return event.as_ptr();
        };
        match event_ref.r#type() {
            NSEventType::LeftMouseDown | NSEventType::RightMouseDown | NSEventType::OtherMouseDown => {
                mouse_down(event_ref, main_thread);
            }
            NSEventType::KeyDown if edit_shortcut(event_ref, main_thread) || browser_shortcut(event_ref, main_thread) => {
                return std::ptr::null_mut();
            }
            _ => {}
        }
        event.as_ptr()
    });
    let mask = NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown | NSEventMask::KeyDown;
    // SAFETY: the handler returns the event it was given, or null to drop
    // a shortcut it handled.
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &handler) };
    std::mem::forget(monitor);
}

fn live_pages() -> Vec<Rc<WebView>> {
    PAGES.with_borrow(|pages| pages.iter().filter_map(Weak::upgrade).collect())
}

/// A click on a page tells its Browser; a click anywhere else gives the
/// keyboard back to GPUI (it doesn't take it back by itself).
fn mouse_down(event: &NSEvent, main_thread: MainThreadMarker) {
    let Some(window) = event.window(main_thread) else {
        return;
    };
    let point = event.locationInWindow();
    let pages = live_pages();
    let clicked = pages.iter().find(|page| page.contains(&window, point));
    match clicked {
        Some(page) => {
            page.events.unbounded_send(WebEvent::Clicked).ok();
        }
        None => {
            let here: Vec<_> = pages.iter().filter(|page| page.is_in(&window)).collect();
            for page in &here {
                page.blur();
            }
            // And if the keyboard ended up with the window itself (a page
            // went away while it had it), GPUI takes it.
            if let Some(page) = here.first()
                && !page.gpui_has_keyboard()
                && !here.iter().any(|page| page.has_keyboard())
            {
                page.give_keyboard_to_gpui();
            }
        }
    }
}

/// ⌘C, ⌘X, ⌘V, ⌘A, ⌘Z and ⇧⌘Z while a page has the keyboard: the app has no
/// Edit menu to send them, so they go to the page from here.
fn edit_shortcut(event: &NSEvent, main_thread: MainThreadMarker) -> bool {
    let flags = event.modifierFlags();
    if !flags.contains(NSEventModifierFlags::Command)
        || flags.intersects(NSEventModifierFlags::Control | NSEventModifierFlags::Option)
    {
        return false;
    }
    let Some(window) = event.window(main_thread) else {
        return false;
    };
    if !live_pages().iter().any(|page| page.is_in(&window) && page.has_keyboard()) {
        return false;
    }
    let key = event.charactersIgnoringModifiers().map(|key| key.to_string().to_lowercase()).unwrap_or_default();
    let shift = flags.contains(NSEventModifierFlags::Shift);
    let action: Sel = match (key.as_str(), shift) {
        ("c", false) => sel!(copy:),
        ("x", false) => sel!(cut:),
        ("v", false) => sel!(paste:),
        ("a", false) => sel!(selectAll:),
        ("z", false) => sel!(undo:),
        ("z", true) => sel!(redo:),
        _ => return false,
    };
    // SAFETY: an editing action to the first responder (the page).
    unsafe { NSApplication::sharedApplication(main_thread).sendAction_to_from(action, None, None) }
}

/// ⌘F, ⌘L, ⌘R, ⌘[ and ⌘] while a page has the keyboard: they're its
/// Browser window's (find, the address bar, reload, back, forward), so they
/// go to it instead of the page.
fn browser_shortcut(event: &NSEvent, main_thread: MainThreadMarker) -> bool {
    let flags = event.modifierFlags();
    if !flags.contains(NSEventModifierFlags::Command)
        || flags.intersects(NSEventModifierFlags::Control | NSEventModifierFlags::Option | NSEventModifierFlags::Shift)
    {
        return false;
    }
    let key = event.charactersIgnoringModifiers().map(|key| key.to_string().to_lowercase()).unwrap_or_default();
    let shortcut = match key.as_str() {
        "f" => Shortcut::Find,
        "l" => Shortcut::FocusAddress,
        "r" => Shortcut::Reload,
        "[" => Shortcut::Back,
        "]" => Shortcut::Forward,
        _ => return false,
    };
    let Some(window) = event.window(main_thread) else {
        return false;
    };
    let pages = live_pages();
    let Some(page) = pages.iter().find(|page| page.is_in(&window) && page.has_keyboard()) else {
        return false;
    };
    page.events.unbounded_send(WebEvent::Shortcut(shortcut)).ok();
    true
}

#[cfg(test)]
mod tests {
    use objc2::AnyThread;
    use objc2_app_kit::NSImage;
    use objc2_foundation::NSData;
    use resvg::tiny_skia;

    /// Snapshots reach GPUI as BGRA, at the window's pixel scale.
    #[test]
    fn snapshots_become_bgra_images_at_the_window_scale() {
        // Red on the left, blue on the right.
        let mut pixmap = tiny_skia::Pixmap::new(4, 2).unwrap();
        for (ix, pixel) in pixmap.pixels_mut().iter_mut().enumerate() {
            let left = ix % 4 < 2;
            *pixel = tiny_skia::PremultipliedColorU8::from_rgba(if left { 255 } else { 0 }, 0, if left { 0 } else { 255 }, 255).unwrap();
        }
        let png = pixmap.encode_png().unwrap();
        let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&png)).unwrap();

        let rendered = super::render_image(&image, 2.).unwrap();
        let size = rendered.size(0);
        assert_eq!((size.width.0, size.height.0), (8, 4), "twice the points at 2×");
        let bytes = rendered.as_bytes(0).unwrap();
        let pixel = |x: usize, y: usize| &bytes[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4];
        assert_eq!(pixel(0, 0), [0, 0, 255, 255], "red, as B G R A");
        assert_eq!(pixel(7, 3), [255, 0, 0, 255], "blue, as B G R A");
    }
}
