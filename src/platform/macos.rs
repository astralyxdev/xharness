//! macOS bits that GPUI doesn't expose: trackpad pinches, the Dock icon, and
//! the server's menu bar icon.

use std::ptr::NonNull;

use block2::RcBlock;
use futures::channel::mpsc;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained, runtime::AnyObject, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSObject, NSSize, NSString};
use resvg::{tiny_skia, usvg};

/// Trackpad pinches, as magnification deltas (0.05 = 5% bigger).
///
/// GPUI never sees macOS magnify events, so this taps them with a local
/// event monitor before they reach it, and passes them on unchanged. Call
/// once, on the main thread; the monitor lives as long as the app.
pub fn pinch_events() -> mpsc::UnboundedReceiver<f32> {
    let (sender, receiver) = mpsc::unbounded();
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit hands the handler a valid event for the call.
        let magnification = unsafe { event.as_ref() }.magnification();
        sender.unbounded_send(magnification as f32).ok();
        event.as_ptr()
    });
    // SAFETY: the handler returns the event it was given, which is valid.
    let monitor =
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::Magnify, &handler) };
    // Removing the monitor needs this object; we never remove it.
    std::mem::forget(monitor);
    receiver
}

/// Sets the Dock icon to `svg`. Unbundled apps (`cargo run`) otherwise get
/// the generic one. Call on the main thread, once the app has started.
pub fn set_app_icon(svg: &[u8]) {
    let Some(png) = app_icon_png(svg) else {
        return;
    };
    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(&png);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    // SAFETY: a valid image, set on the main thread.
    unsafe { NSApplication::sharedApplication(main_thread).setApplicationIconImage(Some(&image)) };
}

/// The logo's X alone, for the menu bar: black on clear, which macOS tints
/// for light and dark menu bars (a template image).
const LOGO_MARK: &str = r#"<svg width="296" height="296" viewBox="0 0 296 296" xmlns="http://www.w3.org/2000/svg"><path d="M0 0H2.5855H35.8437L108 146.859L260.156 0H293.414H296V3.07214V36.8399L131.462 170.208L296 260.136V293.394V296H292.923H259.155L108 193.698L36.8451 296H3.07728H0V293.394V260.136L84.5375 170.208L0 36.8399V3.07216V0Z" fill="black"/></svg>"#;

/// Runs the server (`serve`) with its menu bar icon: AppKit on the main
/// thread (an accessory app: no Dock icon, no menus), the server on
/// another. When the server returns, the process ends, and the icon goes.
pub fn run_server_with_tray(serve: fn() -> std::io::Result<()>) -> ! {
    std::thread::spawn(move || {
        let code = match serve() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("xharness server: {error}");
                1
            }
        };
        std::process::exit(code);
    });
    let Some(main_thread) = MainThreadMarker::new() else {
        loop {
            std::thread::park();
        }
    };
    let app = NSApplication::sharedApplication(main_thread);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    install_status_item(main_thread);
    app.run();
    std::process::exit(0)
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this has no
    // Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "XHarnessServerMenu"]
    struct ServerMenu;

    impl ServerMenu {
        #[unsafe(method(openApp:))]
        fn open_app(&self, _sender: Option<&AnyObject>) {
            open_app();
        }

        #[unsafe(method(stopServer:))]
        fn stop_server(&self, _sender: Option<&AnyObject>) {
            // Ends every shell the server holds, as the app's Stop does.
            std::process::exit(0);
        }
    }
);

impl ServerMenu {
    fn new(main_thread: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(main_thread).set_ivars(());
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

fn install_status_item(main_thread: MainThreadMarker) {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(main_thread)
        && let Some(png) = render_svg(LOGO_MARK.as_bytes(), 36)
    {
        let data = NSData::with_bytes(&png);
        if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
            image.setSize(NSSize::new(18., 18.));
            image.setTemplate(true);
            button.setImage(Some(&image));
        }
        button.setToolTip(Some(&NSString::from_str("XHarness server: terminals and agents keep running here")));
    }
    let target = ServerMenu::new(main_thread);
    let menu = NSMenu::new(main_thread);
    let entry = |title: &str, action: Option<objc2::runtime::Sel>| {
        // SAFETY: a fresh item, with a selector ServerMenu answers (or none).
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(main_thread),
                &NSString::from_str(title),
                action,
                &NSString::from_str(""),
            )
        };
        if action.is_some() {
            // SAFETY: the target lives as long as the app (it's leaked).
            unsafe { item.setTarget(Some(&target)) };
        } else {
            item.setEnabled(false);
        }
        item
    };
    menu.addItem(&entry("XHarness server is running", None));
    menu.addItem(&NSMenuItem::separatorItem(main_thread));
    menu.addItem(&entry("Open XHarness", Some(sel!(openApp:))));
    menu.addItem(&entry("Stop server (ends its terminals)", Some(sel!(stopServer:))));
    item.setMenu(Some(&menu));
    // They live as long as the process.
    std::mem::forget(item);
    std::mem::forget(menu);
    std::mem::forget(target);
}

/// Opens the app: its bundle if the server runs from one, else the same
/// binary.
fn open_app() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let bundle = exe.ancestors().find(|path| path.extension().is_some_and(|ext| ext == "app"));
    let mut command = match bundle {
        Some(bundle) => {
            let mut open = std::process::Command::new("open");
            open.arg(bundle);
            open
        }
        None => {
            let mut app = std::process::Command::new(&exe);
            // The server's AppKit ignores SIGTERM (it takes it as a quit
            // request), and ignored signals are inherited: the app would
            // ignore them too. SAFETY: signal() is async-signal-safe.
            unsafe {
                use std::os::unix::process::CommandExt;
                app.pre_exec(|| {
                    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                    Ok(())
                });
            }
            app
        }
    };
    let spawned = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // Reaped when it quits, so it doesn't linger as a zombie.
    if let Ok(mut app) = spawned {
        std::thread::spawn(move || app.wait().ok());
    }
}

/// `svg` drawn `size` px square, as a PNG.
fn render_svg(svg: &[u8], size: u32) -> Option<Vec<u8>> {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.encode_png().ok()
}

/// The logo on the macOS icon grid: an 824px rounded square in the middle
/// of a 1024px canvas, the way Dock icons are drawn.
pub fn app_icon_png(svg: &[u8]) -> Option<Vec<u8>> {
    const CANVAS: u32 = 1024;
    const ICON: f32 = 824.;
    const RADIUS: f32 = 185.;
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let mut logo = tiny_skia::Pixmap::new(ICON as u32, ICON as u32)?;
    let scale = ICON / tree.size().width().max(tree.size().height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut logo.as_mut());

    let offset = (CANVAS as f32 - ICON) / 2.;
    let mut mask = tiny_skia::Mask::new(CANVAS, CANVAS)?;
    let shape = rounded_square(offset, ICON, RADIUS)?;
    mask.fill_path(&shape, tiny_skia::FillRule::Winding, true, tiny_skia::Transform::identity());
    let mut canvas = tiny_skia::Pixmap::new(CANVAS, CANVAS)?;
    canvas.draw_pixmap(
        offset as i32,
        offset as i32,
        logo.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        Some(&mask),
    );
    canvas.encode_png().ok()
}

fn rounded_square(origin: f32, size: f32, radius: f32) -> Option<tiny_skia::Path> {
    // Cubic Béziers approximating quarter circles.
    let k = radius * 0.552_284_8;
    let (left, top, right, bottom) = (origin, origin, origin + size, origin + size);
    let mut path = tiny_skia::PathBuilder::new();
    path.move_to(left + radius, top);
    path.line_to(right - radius, top);
    path.cubic_to(right - radius + k, top, right, top + radius - k, right, top + radius);
    path.line_to(right, bottom - radius);
    path.cubic_to(right, bottom - radius + k, right - radius + k, bottom, right - radius, bottom);
    path.line_to(left + radius, bottom);
    path.cubic_to(left + radius - k, bottom, left, bottom - radius + k, left, bottom - radius);
    path.line_to(left, top + radius);
    path.cubic_to(left, top + radius - k, left + radius - k, top, left + radius, top);
    path.close();
    path.finish()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_logo_renders_as_an_app_icon() {
        let png = super::app_icon_png(crate::ui::assets::LOGO).expect("renders");
        assert_eq!(&png[1..4], b"PNG");
    }
}

