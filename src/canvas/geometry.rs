//! The canvas's arithmetic: resizing from an edge, zooming around a point,
//! fitting windows in the view.

use gpui::{CursorStyle, Point};

use crate::services::projects::{Frame, MAX_ZOOM, MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH, MIN_ZOOM, Viewport};

/// Room left around windows when zooming to fit them.
const FIT_PADDING: f32 = 48.;

/// Which side(s) of a window a resize handle moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Edge {
    pub const ALL: [Edge; 8] = [
        Edge::Left,
        Edge::Right,
        Edge::Top,
        Edge::Bottom,
        Edge::TopLeft,
        Edge::TopRight,
        Edge::BottomLeft,
        Edge::BottomRight,
    ];

    fn left(self) -> bool {
        matches!(self, Edge::Left | Edge::TopLeft | Edge::BottomLeft)
    }

    fn right(self) -> bool {
        matches!(self, Edge::Right | Edge::TopRight | Edge::BottomRight)
    }

    fn top(self) -> bool {
        matches!(self, Edge::Top | Edge::TopLeft | Edge::TopRight)
    }

    fn bottom(self) -> bool {
        matches!(self, Edge::Bottom | Edge::BottomLeft | Edge::BottomRight)
    }

    pub fn cursor(self) -> CursorStyle {
        match self {
            Edge::Left | Edge::Right => CursorStyle::ResizeLeftRight,
            Edge::Top | Edge::Bottom => CursorStyle::ResizeUpDown,
            Edge::TopLeft | Edge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
            Edge::TopRight | Edge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
        }
    }
}

/// `frame` resized by dragging `edge` by (`dx`, `dy`) canvas units. The
/// opposite side stays put, and the window never gets smaller than the
/// minimum.
pub fn resized(frame: Frame, edge: Edge, dx: f32, dy: f32) -> Frame {
    resized_to_min(frame, edge, dx, dy, (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT))
}

/// [`resized`], with its own minimum size (a mark's).
pub fn resized_to_min(frame: Frame, edge: Edge, dx: f32, dy: f32, (min_width, min_height): (f32, f32)) -> Frame {
    let mut next = frame;
    if edge.left() {
        let right = frame.x + frame.width;
        next.x = (frame.x + dx).min(right - min_width);
        next.width = right - next.x;
    }
    if edge.right() {
        next.width = (frame.width + dx).max(min_width);
    }
    if edge.top() {
        let bottom = frame.y + frame.height;
        next.y = (frame.y + dy).min(bottom - min_height);
        next.height = bottom - next.y;
    }
    if edge.bottom() {
        next.height = (frame.height + dy).max(min_height);
    }
    next
}

/// The viewport after zooming to `zoom` while keeping the canvas point
/// under `anchor` (view coordinates, in px) where it is.
pub fn zoom_around(viewport: Viewport, anchor: Point<f32>, zoom: f32) -> Viewport {
    let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
    let canvas_x = viewport.x + anchor.x / viewport.zoom;
    let canvas_y = viewport.y + anchor.y / viewport.zoom;
    Viewport {
        x: canvas_x - anchor.x / zoom,
        y: canvas_y - anchor.y / zoom,
        zoom,
    }
}

/// The viewport that fits `frames` in a view of `view` px, at most `max_zoom`.
pub fn fit(frames: &[Frame], view: (f32, f32), max_zoom: f32) -> Option<Viewport> {
    let (min_x, min_y, max_x, max_y) = frames.iter().fold(
        (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY),
        |(a, b, c, d), f| (a.min(f.x), b.min(f.y), c.max(f.x + f.width), d.max(f.y + f.height)),
    );
    if frames.is_empty() || view.0 <= 0. || view.1 <= 0. {
        return None;
    }
    let (width, height) = (max_x - min_x, max_y - min_y);
    let zoom = ((view.0 - 2. * FIT_PADDING) / width)
        .min((view.1 - 2. * FIT_PADDING) / height)
        .clamp(MIN_ZOOM, max_zoom);
    Some(Viewport {
        x: min_x + width / 2. - view.0 / 2. / zoom,
        y: min_y + height / 2. - view.1 / 2. / zoom,
        zoom,
    })
}
