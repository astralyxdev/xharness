//! Native pages over GPUI's drawing.
//!
//! A Browser window's page is a native view above everything GPUI draws
//! (see `platform/webview.rs`), so GPUI can't paint over it: a window
//! dragged on top, a menu, a dialog would all show behind the page. So
//! every frame, in paint order, pages say where they are ([`page`]) and
//! whatever GPUI draws that could cover one says where it is
//! ([`occluder`]). At the end of the frame ([`finish`], painted last) each
//! page is shown where it was painted, or hidden if something painted
//! after it overlaps it; the Browser window then shows a snapshot of the
//! page in its place. Pages that weren't painted at all (another project
//! is showing, the window scrolled out of view) are hidden too.

use gpui::{App, Bounds, Pixels, Window, WindowId, canvas, prelude::*};

#[cfg(target_os = "macos")]
use std::rc::{Rc, Weak};

#[cfg(target_os = "macos")]
use crate::platform::webview::{Placement, WebView};

#[derive(Default)]
struct Layers {
    frames: std::collections::HashMap<WindowId, Frame>,
}

impl gpui::Global for Layers {}

#[derive(Default)]
struct Frame {
    painted: Vec<Painted>,
    /// Pages shown after the last frame.
    #[cfg(target_os = "macos")]
    shown: Vec<Weak<WebView>>,
}

// Elsewhere there are no pages to place, so nothing reads these.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum Painted {
    Occluder(Bounds<Pixels>),
    #[cfg(target_os = "macos")]
    Page {
        view: Rc<WebView>,
        page: Bounds<Pixels>,
        clip: Bounds<Pixels>,
        zoom: f32,
        corner_radius: f32,
        interactive: bool,
        at_window_edges: bool,
    },
}

fn frame<'a>(window: &Window, cx: &'a mut App) -> &'a mut Frame {
    let id = window.window_handle().window_id();
    cx.default_global::<Layers>().frames.entry(id).or_default()
}

/// Something GPUI draws that a page shouldn't show through: fills its
/// parent (which must be positioned) and records it when painted.
pub fn occluder() -> impl IntoElement {
    canvas(|_, _, _| {}, |bounds, _, window, cx| {
        // Where it's drawn, inside a zoomed window too.
        let bounds = window.element_transform().apply_bounds(bounds);
        frame(window, cx).painted.push(Painted::Occluder(bounds))
    })
        .absolute()
        .top_0()
        .left_0()
        .size_full()
}

/// How a page is painted (see [`page`]).
#[cfg(target_os = "macos")]
pub struct PagePaint {
    /// Where, in window coordinates.
    pub bounds: Bounds<Pixels>,
    /// Screen px per CSS px.
    pub zoom: f32,
    pub corner_radius: f32,
    /// In the window you're working in (only that one takes the mouse).
    pub interactive: bool,
    /// Its left, right and bottom edges are the window's.
    pub at_window_edges: bool,
}

/// A page painted this frame, visible within the current clip. `paint` is
/// in the units it was laid out in; a zoomed window (see the canvas) draws
/// it scaled, and so is the page.
#[cfg(target_os = "macos")]
pub fn page(view: Rc<WebView>, paint: PagePaint, window: &mut Window, cx: &mut App) {
    view.attach(window);
    let transform = window.element_transform();
    let clip = transform.apply_bounds(window.content_mask().bounds);
    frame(window, cx).painted.push(Painted::Page {
        view,
        page: transform.apply_bounds(paint.bounds),
        clip,
        zoom: paint.zoom * transform.scale,
        corner_radius: paint.corner_radius * transform.scale,
        interactive: paint.interactive,
        at_window_edges: paint.at_window_edges,
    });
}

/// Ends the frame: places every page painted in it, and hides the rest.
/// Paint it after everything else (see [`sweeper`]).
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn finish(window: &mut Window, cx: &mut App) {
    let frame = frame(window, cx);
    let painted = std::mem::take(&mut frame.painted);
    #[cfg(target_os = "macos")]
    {
        let mut shown = Vec::new();
        for (ix, entry) in painted.iter().enumerate() {
            let Painted::Page { view, page, clip, zoom, corner_radius, interactive, at_window_edges } = entry else {
                continue;
            };
            let visible = page.intersect(clip);
            let empty = visible.size.width <= Pixels::ZERO || visible.size.height <= Pixels::ZERO;
            let covered = !empty
                && painted[ix + 1..]
                    .iter()
                    .any(|later| matches!(later, Painted::Occluder(bounds) if bounds.intersects(&visible)));
            if empty {
                view.place(None);
                continue;
            }
            if covered {
                // What it looked like, to show in its place.
                if view.is_shown() {
                    view.snapshot();
                }
                view.place(None);
                continue;
            }
            view.place(Some(Placement {
                visible,
                page: *page,
                zoom: *zoom,
                corner_radius: *corner_radius,
                interactive: *interactive,
                at_window_edges: *at_window_edges,
            }));
            shown.push(Rc::downgrade(view));
        }
        for gone in std::mem::replace(&mut frame.shown, shown.clone()) {
            if let Some(view) = gone.upgrade()
                && !shown.iter().any(|still| Weak::ptr_eq(still, &gone))
            {
                view.place(None);
            }
        }
    }
    drop(painted);
}

/// The element that ends each frame: deferred, at the highest priority, so
/// it paints after everything (menus included).
pub fn sweeper() -> impl IntoElement {
    gpui::deferred(canvas(|_, _, _| {}, |_, _, window, cx| finish(window, cx)).absolute().size_full())
        .with_priority(usize::MAX)
}
