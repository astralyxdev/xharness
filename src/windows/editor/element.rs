//! Paints the visible lines with their numbers, the selection and caret,
//! diagnostics, and a scrollbar.

use super::*;

/// Paints the visible lines with their numbers, the selection and caret,
/// and a scrollbar.
pub(super) struct EditorElement {
    pub(super) editor: Entity<EditorView>,
}

pub(super) struct EditorPrepaint {
    pub(super) text_bounds: Bounds<Pixels>,
    pub(super) gutter_bounds: Bounds<Pixels>,
    pub(super) line_height: Pixels,
    /// Behind the text: the caret's line, the selection.
    pub(super) quads: Vec<PaintQuad>,
    pub(super) lines: Vec<(Point<Pixels>, ShapedLine)>,
    pub(super) numbers: Vec<(Point<Pixels>, ShapedLine)>,
    /// Diagnostics: squiggles and messages over the text, dots in the
    /// gutter.
    pub(super) squiggles: Vec<(gpui::Path<Pixels>, Hsla)>,
    pub(super) messages: Vec<(Point<Pixels>, ShapedLine)>,
    pub(super) marks: Vec<PaintQuad>,
    /// On top: the caret, the IME underline, the scrollbar.
    pub(super) overlays: Vec<PaintQuad>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
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
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = Theme::global(cx).clone();
        let zoom = window.rem_size() / px(16.);
        let font_size = px(FONT_SIZE * zoom);
        let line_height = px(LINE_HEIGHT * zoom);
        let base = font(FONT_MONO);
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&base);
        let char_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|advance| advance.width)
            .unwrap_or(font_size * 0.6);
        let focused = self.editor.read(cx).focus_handle.is_focused(window);

        let line_count = self.editor.read(cx).buffer.len();
        let digits = line_count.to_string().len().max(3);
        let gutter_width = char_width * (digits as f32 + 3.);
        let gutter_bounds = Bounds::new(bounds.origin, size(gutter_width, bounds.size.height));
        let text_bounds = Bounds::from_corners(point(bounds.left() + gutter_width, bounds.top()), bounds.bottom_right());
        let padding = px(4. * zoom);
        self.editor.update(cx, |editor, _| {
            editor.update_scroll(size(text_bounds.size.width - padding, text_bounds.size.height), line_height, char_width)
        });

        let editor = self.editor.read(cx);
        let origin = point(
            text_bounds.left() + padding - px(editor.scroll_left * zoom),
            bounds.top() - line_height * editor.scroll_top,
        );
        let first = editor.scroll_top.floor() as usize;
        let rows = (bounds.size.height / line_height).ceil() as usize + 1;
        let last = (first + rows).min(line_count);
        let head = editor.selection.head;
        let selection = editor.selection.range();

        let mut shaped: Vec<(usize, Option<ShapedLine>)> = Vec::new();
        let mut lines = Vec::new();
        let mut numbers = Vec::new();
        for ix in first..last {
            let line = editor.buffer.line(ix);
            let y = origin.y + line_height * ix as f32;
            let shaped_line = (!line.is_empty()).then(|| {
                let display = display_text(line);
                let runs = editor.runs(ix, line, display.len(), &base, &theme);
                text_system.shape_line(SharedString::from(display), font_size, &runs, None)
            });
            if let Some(shaped_line) = &shaped_line {
                lines.push((point(origin.x, y), shaped_line.clone()));
            }
            shaped.push((ix, shaped_line));

            let number = (ix + 1).to_string();
            let color = if ix == head.line { theme.foreground } else { theme.muted_foreground.opacity(0.6) };
            let run = TextRun {
                len: number.len(),
                font: base.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let number = text_system.shape_line(SharedString::from(number), font_size, &[run], None);
            let x = gutter_bounds.right() - char_width * 1.5 - number.width;
            numbers.push((point(x, y), number));
        }

        let x_at = |ix: usize, col: usize| -> Pixels {
            match shaped.iter().find(|(line, _)| *line == ix) {
                Some((_, Some(line))) => line.x_for_index(to_display(editor.buffer.line(ix), col)),
                _ => px(0.),
            }
        };
        let width_of = |ix: usize| -> Pixels {
            match shaped.iter().find(|(line, _)| *line == ix) {
                Some((_, Some(line))) => line.width,
                _ => px(0.),
            }
        };

        let mut quads = Vec::new();
        let head_y = origin.y + line_height * head.line as f32;
        if editor.selection.is_empty() && focused {
            quads.push(fill(
                Bounds::new(point(text_bounds.left(), head_y), size(text_bounds.size.width, line_height)),
                theme.foreground.opacity(0.04),
            ));
        }
        let selection_color = theme.tints.blue.base.opacity(if focused { 0.3 } else { 0.15 });
        for ix in selection.start.line.max(first)..=selection.end.line.min(last.saturating_sub(1)) {
            if editor.selection.is_empty() || ix >= last {
                break;
            }
            let start = if ix == selection.start.line { x_at(ix, selection.start.col) } else { px(0.) };
            let end = if ix == selection.end.line {
                x_at(ix, selection.end.col)
            } else {
                // The line break, selected.
                width_of(ix) + char_width * 0.6
            };
            let y = origin.y + line_height * ix as f32;
            quads.push(fill(
                Bounds::from_corners(point(origin.x + start, y), point(origin.x + end, y + line_height)),
                selection_color,
            ));
        }

        // Diagnostics on the visible lines (hints aren't drawn).
        let (mut squiggles, mut messages, mut marks) = (Vec::new(), Vec::new(), Vec::new());
        let mut worst: Vec<Option<&lsp::Diagnostic>> = vec![None; last.saturating_sub(first)];
        for diagnostic in editor.diagnostics.iter().filter(|d| d.severity <= 3) {
            let (start_line, end_line) = (diagnostic.start.0, diagnostic.end.0.max(diagnostic.start.0));
            if end_line < first || start_line >= last {
                continue;
            }
            let color = severity_color(diagnostic.severity, &theme);
            for ix in start_line.max(first)..=end_line.min(last - 1) {
                let line = editor.buffer.line(ix);
                let start = if ix == start_line { utf16_to_byte(line, diagnostic.start.1) } else { 0 };
                let end = if ix == end_line { utf16_to_byte(line, diagnostic.end.1) } else { line.len() };
                let (from, mut to) = (x_at(ix, start), x_at(ix, end));
                if to <= from {
                    // An empty range: mark one character's width.
                    to = from + char_width;
                }
                let y = origin.y + line_height * (ix + 1) as f32 - px(2. * zoom);
                if let Some(path) = squiggle(origin.x + from, origin.x + to, y, zoom) {
                    squiggles.push((path, color));
                }
            }
            if (first..last).contains(&start_line) {
                let slot = &mut worst[start_line - first];
                if slot.is_none_or(|d| diagnostic.severity < d.severity) {
                    *slot = Some(diagnostic);
                }
            }
        }
        for (offset, diagnostic) in worst.into_iter().enumerate() {
            let Some(diagnostic) = diagnostic else {
                continue;
            };
            let ix = first + offset;
            let y = origin.y + line_height * ix as f32;
            let color = severity_color(diagnostic.severity, &theme);
            // The message at the end of the line, as in Zed's inline
            // diagnostics.
            let text: String = diagnostic.message.lines().next().unwrap_or_default().chars().take(160).collect();
            if !text.is_empty() {
                let run = TextRun {
                    len: text.len(),
                    font: base.clone(),
                    color: color.opacity(0.85),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = text_system.shape_line(SharedString::from(text), font_size * 0.92, &[run], None);
                messages.push((point(origin.x + width_of(ix) + char_width * 3., y), shaped));
            }
            let dot = px(6. * zoom);
            marks.push(
                fill(
                    Bounds::new(point(gutter_bounds.left() + char_width * 0.6, y + (line_height - dot) / 2.), size(dot, dot)),
                    color,
                )
                .corner_radii(dot / 2.),
            );
        }

        let mut overlays = Vec::new();
        let visible = (first..last).contains(&head.line);
        let caret_point = visible.then(|| point(origin.x + x_at(head.line, head.col), head_y + line_height));
        if focused && visible {
            let x = origin.x + x_at(head.line, head.col);
            let stroke = px(2. * zoom).max(px(1.));
            overlays.push(fill(Bounds::new(point(x, head_y), size(stroke, line_height)), theme.foreground));
            if let Some(marked) = &editor.marked {
                let (start, end) = (x_at(head.line, marked.start), x_at(head.line, marked.end));
                overlays.push(fill(
                    Bounds::new(point(origin.x + start, head_y + line_height - px(2.)), size(end - start, px(1.))),
                    theme.foreground,
                ));
            }
        }
        // The scrollbar: a thumb, while the file is longer than the view.
        let visible_rows = bounds.size.height / line_height;
        if line_count as f32 > visible_rows {
            let total = line_count as f32 + visible_rows - 1.;
            let thumb = (bounds.size.height * (visible_rows / total)).max(px(24. * zoom));
            let max_top = (line_count as f32 - 1.).max(1.);
            let y = bounds.top() + (bounds.size.height - thumb) * (editor.scroll_top / max_top);
            let width = px(6. * zoom);
            overlays.push(
                fill(
                    Bounds::new(point(bounds.right() - width - px(2. * zoom), y), size(width, thumb)),
                    theme.foreground.opacity(0.15),
                )
                .corner_radii(width / 2.),
            );
        }

        self.editor.update(cx, |editor, _| {
            editor.shaped = shaped;
            editor.caret_point = caret_point;
            editor.layout = Some(Layout {
                origin,
                line_height,
                char_width,
            });
        });

        EditorPrepaint {
            text_bounds,
            gutter_bounds,
            line_height,
            quads,
            lines,
            numbers,
            squiggles,
            messages,
            marks,
            overlays,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.editor.clone()), cx);

        // Dragging a selection keeps going outside the text.
        let editor = self.editor.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && event.pressed_button == Some(MouseButton::Left) {
                editor.update(cx, |editor, cx| editor.drag(event.position, cx));
            }
        });
        let editor = self.editor.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                editor.update(cx, |editor, _| editor.selecting = false);
            }
        });

        let line_height = prepaint.line_height;
        window.with_content_mask(Some(ContentMask { bounds: prepaint.text_bounds }), |window| {
            for quad in prepaint.quads.drain(..) {
                window.paint_quad(quad);
            }
            for (origin, line) in prepaint.lines.drain(..) {
                line.paint(origin, line_height, window, cx).ok();
            }
            for (path, color) in prepaint.squiggles.drain(..) {
                window.paint_path(path, color);
            }
            for (origin, message) in prepaint.messages.drain(..) {
                message.paint(origin, line_height, window, cx).ok();
            }
        });
        window.with_content_mask(Some(ContentMask { bounds: prepaint.gutter_bounds }), |window| {
            for (origin, number) in prepaint.numbers.drain(..) {
                number.paint(origin, line_height, window, cx).ok();
            }
            for mark in prepaint.marks.drain(..) {
                window.paint_quad(mark);
            }
        });
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in prepaint.overlays.drain(..) {
                window.paint_quad(quad);
            }
        });
    }
}
