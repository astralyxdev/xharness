//! Dragging files out of the app: an Explorer's rows onto Finder, Mail, a
//! browser's upload box. GPUI only drags inside its own window, so when a
//! drag of files leaves the window it's handed to AppKit as a real file drag
//! (file URLs on the drag pasteboard, each file's icon under the pointer).
//! Other apps get a copy: the source never offers to move, so dropping on
//! Finder on the same disk doesn't take the file out of the project.

use std::{cell::RefCell, path::PathBuf};

use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEventType,
    NSView, NSWorkspace,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};

thread_local! {
    /// Sources of drags in flight (AppKit doesn't keep them alive).
    static SOURCES: RefCell<Vec<Retained<DragSource>>> = const { RefCell::new(Vec::new()) };
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this doesn't
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "XHarnessFileDragSource"]
    struct DragSource;

    unsafe impl NSObjectProtocol for DragSource {}

    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operations(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, _operation: NSDragOperation) {
            let this: *const Self = self;
            SOURCES.with(|sources| sources.borrow_mut().retain(|source| !std::ptr::eq(&**source, this)));
        }
    }
);

impl DragSource {
    fn new(main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(());
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

/// Starts dragging `paths` (files on this Mac) out of `window`, from the
/// mouse event being handled. Returns whether it started (it needs a
/// mouse-drag event, and files that exist).
pub fn begin(window: &gpui::Window, paths: &[PathBuf]) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Some(main_thread) = MainThreadMarker::new() else {
        return false;
    };
    if paths.is_empty() || !paths.iter().all(|path| path.exists()) {
        return false;
    }
    let app = NSApplication::sharedApplication(main_thread);
    let Some(event) = app.currentEvent() else {
        return false;
    };
    if !matches!(event.r#type(), NSEventType::LeftMouseDragged) {
        return false;
    }
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return false;
    };
    // SAFETY: GPUI's view for this window, alive while the window is.
    let Some(view) = (unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }) else {
        return false;
    };
    let pointer = view.convertPoint_fromView(event.locationInWindow(), None);
    let workspace = NSWorkspace::sharedWorkspace();
    let items: Vec<Retained<NSDraggingItem>> = paths
        .iter()
        .enumerate()
        .map(|(ix, path)| {
            let path = NSString::from_str(&path.to_string_lossy());
            let url = NSURL::fileURLWithPath(&path);
            let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*url));
            let icon = workspace.iconForFile(&path);
            // Fanned out a little when there are several.
            let offset = (ix.min(4) as f64) * 6.;
            let frame = NSRect::new(NSPoint::new(pointer.x - 20. + offset, pointer.y - 20. - offset), NSSize::new(40., 40.));
            // SAFETY: an NSImage is valid dragging contents.
            unsafe { item.setDraggingFrame_contents(frame, Some(&icon)) };
            item
        })
        .collect();
    let items = NSArray::from_retained_slice(&items);
    let source = DragSource::new(main_thread);
    let session = view.beginDraggingSessionWithItems_event_source(&items, &event, ProtocolObject::from_ref(&*source));
    session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    SOURCES.with(|sources| sources.borrow_mut().push(source));
    true
}
