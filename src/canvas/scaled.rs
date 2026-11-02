//! Zooming windows: their contents are laid out at 100% and drawn scaled.

use gpui::{AnyElement, App, Bounds, ElementId, GlobalElementId, LayoutId, Pixels, Style, Window, prelude::*, px, size};

/// Lays its child out at `size` (the window's size at 100%) and draws it
/// zoomed by `zoom`, filling its own bounds (`size` × `zoom`). Zoom changes
/// nothing inside the window — text, wrapping, a terminal's rows — only how
/// big it's drawn, like a picture (see GPUI's `Window::with_zoom`).
pub struct Zoomed {
    pub size: (f32, f32),
    pub zoom: f32,
    pub child: AnyElement,
}

impl IntoElement for Zoomed {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Zoomed {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
        // The child (sized `size` itself) in a box of the zoomed size: it
        // overflows the box, or doesn't fill it, until it's drawn zoomed.
        let child = self.child.request_layout(window, cx);
        let (width, height) = self.size;
        let mut style = Style::default();
        style.size = size(px(width * self.zoom).into(), px(height * self.zoom).into());
        (window.request_layout(style, [child], cx), ())
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
        let child = &mut self.child;
        window.with_zoom(bounds.origin, self.zoom, |window| child.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        window.with_zoom(bounds.origin, self.zoom, |window| child.paint(window, cx));
    }
}
