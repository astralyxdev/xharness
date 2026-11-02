//! Marks: text, notes and sections drawn on a canvas to mark it up (see
//! [`MarkKind`]). They aren't windows: the sidebar doesn't list them, they
//! run nothing, and each moves on its own. Sections sit behind the windows
//! and carry what's inside them when moved; text and notes sit in front.
//!
//! The canvas's tools (bottom of the view) add them: pick Text or Note and
//! click, or Section and drag out an area. A click selects a mark (its
//! toolbar picks a color, a text size, duplicates or deletes it); a
//! double-click edits its text. Like windows, marks are laid out at 100%
//! and drawn zoomed.

use gpui::{Entity, Focusable, MouseUpEvent, TextRun};

use super::*;
use crate::i18n::t;
use crate::services::projects::{Mark, MarkChange, MarkKind, ProjectColor, TEXT_SIZES};
use crate::ui::text_input::{self, TextInput, TextInputEvent};
use crate::ui::theme::Tint;

actions!(marks, [NewLine, LineUp, LineDown, FinishEditing]);

/// Key context of a mark being edited.
const EDITOR_CONTEXT: &str = "MarkEditor";
/// Room above a section for its name.
pub(super) const SECTION_TITLE_HEIGHT: f32 = 28.;
/// Padding around a Text mark's text.
const TEXT_PADDING: (f32, f32) = (6., 2.);

pub fn bind_keys(cx: &mut App) {
    let context = Some(EDITOR_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("enter", NewLine, context),
        KeyBinding::new("up", LineUp, context),
        KeyBinding::new("down", LineDown, context),
        KeyBinding::new("escape", FinishEditing, context),
        KeyBinding::new("secondary-enter", FinishEditing, context),
    ]);
}

/// What a click on empty canvas does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// Picks and moves things; dragging empty canvas pans.
    Select,
    /// Dragging anywhere pans, over windows too.
    Hand,
    /// Adds a mark where you click (a section: where you drag).
    Add(MarkKind),
}

/// A mark whose text is being edited: a field per line.
pub(super) struct MarkEditing {
    pub mark: u64,
    lines: Vec<Entity<TextInput>>,
    _subscriptions: Vec<Subscription>,
}

/// Dragging a mark (a note or text, or a section by its name).
pub struct MoveMark(pub u64);

/// Dragging one of a selected mark's resize handles.
pub struct ResizeMark {
    mark: u64,
    edge: Edge,
}

impl ResizeMark {
    pub(super) fn target(&self) -> (u64, Edge) {
        (self.mark, self.edge)
    }
}

/// Dragging out a new section.
pub struct DrawSection;

/// Where a mark drag started, and what a section carries along.
pub(super) struct MarkGesture {
    mouse: Point<Pixels>,
    zoom: f32,
    start: Mark,
    windows: Vec<(u64, f32, f32)>,
    marks: Vec<(u64, f32, f32)>,
}

/// A mark on screen: where (relative to the canvas view) and how big, in px.
#[derive(Clone, Copy)]
struct Placed {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
}

/// The accent family for `color` (`None` for gray).
fn tint(color: ProjectColor, theme: &Theme) -> Option<Tint> {
    let tints = &theme.tints;
    Some(match color {
        ProjectColor::Gray => return None,
        ProjectColor::Blue => tints.blue,
        ProjectColor::Violet => tints.violet,
        ProjectColor::Cyan => tints.cyan,
        ProjectColor::Green => tints.green,
        ProjectColor::Amber => tints.amber,
        ProjectColor::Rose => tints.rose,
    })
}

/// A note line that's a checkbox: `[ ] text` or `[x] text`.
fn checkbox(line: &str) -> Option<(bool, &str)> {
    let rest = line.trim_start();
    for (prefix, done) in [("[ ]", false), ("[x]", true), ("[X]", true), ("- [ ]", false), ("- [x]", true)] {
        if let Some(text) = rest.strip_prefix(prefix) {
            return Some((done, text.trim_start()));
        }
    }
    None
}

/// `text` with line `index`'s checkbox ticked or cleared.
fn toggle_checkbox(text: &str, index: usize) -> String {
    text.split('\n')
        .enumerate()
        .map(|(ix, line)| {
            if ix != index {
                return line.to_string();
            }
            if let Some(at) = line.find("[ ]") {
                format!("{}[x]{}", &line[..at], &line[at + 3..])
            } else if let Some(at) = line.find("[x]").or_else(|| line.find("[X]")) {
                format!("{}[ ]{}", &line[..at], &line[at + 3..])
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl ProjectCanvas {
    // Tools ------------------------------------------------------------------

    pub fn set_tool(&mut self, tool: Tool, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        self.tool = tool;
        if tool != Tool::Select {
            self.selected_mark = None;
        }
        // Single-letter tool keys and Delete work on the canvas itself.
        self.active = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Adds a mark in the middle of the view (from the command palette).
    pub fn add_mark_in_view(&mut self, kind: MarkKind, window: &mut Window, cx: &mut Context<Self>) {
        let viewport = self.viewport(cx);
        let (width, height) = self.view_size();
        let (mark_w, mark_h) = kind.default_size();
        let x = viewport.x + (width / viewport.zoom - mark_w).max(0.) / 2.;
        let y = viewport.y + (height / viewport.zoom - mark_h).max(0.) / 2.;
        self.add_mark_at(kind, x, y, None, window, cx);
    }

    /// Adds a mark with its top-left corner at (`x`, `y`), in canvas units,
    /// optionally sized, selects it and starts editing its text.
    fn add_mark_at(&mut self, kind: MarkKind, x: f32, y: f32, size: Option<(f32, f32)>, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project_id;
        let text = match kind {
            MarkKind::Section => {
                let number = self.project(cx).map_or(0, |p| p.marks.iter().filter(|m| m.kind == MarkKind::Section).count()) + 1;
                format!("{} {number}", t("Section"))
            }
            _ => String::new(),
        };
        let (x, y) = (settings::snap(x, cx), settings::snap(y, cx));
        let id = self.store.update(cx, |store, _| {
            let id = store.add_mark(project, kind, x, y, text)?;
            if let Some((width, height)) = size {
                store.update_mark(project, id, MarkChange::Bounds { x, y, width, height });
            }
            Some(id)
        });
        let Some(id) = id else {
            return;
        };
        self.save_soon(cx);
        self.tool = Tool::Select;
        self.selected_mark = Some(id);
        self.active = None;
        if kind == MarkKind::Section {
            window.focus(&self.focus_handle);
        } else {
            self.start_editing(id, window, cx);
        }
        cx.notify();
    }

    fn mark(&self, id: u64, cx: &App) -> Option<Mark> {
        self.project(cx)?.marks.iter().find(|m| m.id == id).cloned()
    }

    /// Picks a mark (its toolbar shows), letting go of the active window.
    fn select_mark(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.as_ref().is_some_and(|e| e.mark != id) {
            self.finish_editing(cx);
        }
        self.selected_mark = Some(id);
        self.active = None;
        let project = self.project_id;
        self.store.update(cx, |store, _| store.raise_mark(project, id));
        if self.editing.is_none() {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    pub(super) fn delete_selected_mark(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.selected_mark.take() else {
            return false;
        };
        if self.editing.as_ref().is_some_and(|e| e.mark == id) {
            self.editing = None;
        }
        let project = self.project_id;
        self.store.update(cx, |store, cx| {
            store.remove_mark(project, id);
            cx.notify();
        });
        cx.notify();
        true
    }

    fn duplicate_mark(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project_id;
        if let Some(copy) = self.store.update(cx, |store, _| store.duplicate_mark(project, id)) {
            self.select_mark(copy, window, cx);
        }
    }

    fn change_mark(&mut self, id: u64, change: MarkChange, cx: &mut Context<Self>) {
        let project = self.project_id;
        self.store.update(cx, |store, cx| {
            store.update_mark(project, id, change);
            cx.notify();
        });
        self.save_soon(cx);
        cx.notify();
    }

    // Editing ------------------------------------------------------------------

    /// Turns a mark's text into fields, the caret at the end.
    fn start_editing(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mark) = self.mark(id, cx) else {
            return;
        };
        self.finish_editing(cx);
        let placeholder = match mark.kind {
            MarkKind::Text => t("Type something"),
            MarkKind::Note => t("Write a note…"),
            MarkKind::Section => t("Section name"),
        };
        let lines: Vec<String> = if mark.kind == MarkKind::Section {
            vec![mark.text.replace('\n', " ")]
        } else {
            mark.text.split('\n').map(str::to_string).collect()
        };
        let mut editing = MarkEditing {
            mark: id,
            lines: Vec::new(),
            _subscriptions: Vec::new(),
        };
        for (ix, line) in lines.into_iter().enumerate() {
            let placeholder = if ix == 0 { placeholder } else { "" };
            let input = self.line_input(line, placeholder, cx);
            editing._subscriptions.push(cx.subscribe(&input, Self::line_changed));
            editing.lines.push(input);
        }
        if let Some(last) = editing.lines.last() {
            last.update(cx, |input, cx| {
                let end = input.text().len();
                input.set_cursor(end, cx);
            });
            window.focus(&last.focus_handle(cx));
        }
        self.selected_mark = Some(id);
        self.editing = Some(editing);
        cx.notify();
    }

    fn line_input(&self, text: String, placeholder: &str, cx: &mut Context<Self>) -> Entity<TextInput> {
        let placeholder = placeholder.to_string();
        cx.new(|cx| TextInput::new(placeholder, cx).inline().line_of_many().with_text(text))
    }

    fn line_changed(&mut self, _: Entity<TextInput>, _: &TextInputEvent, cx: &mut Context<Self>) {
        self.write_back(cx);
    }

    /// Copies the fields' text into the mark.
    fn write_back(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = &self.editing else {
            return;
        };
        let text = editing.lines.iter().map(|line| line.read(cx).text().to_string()).collect::<Vec<_>>().join("\n");
        let (project, id) = (self.project_id, editing.mark);
        self.store.update(cx, |store, _| store.update_mark(project, id, MarkChange::Text(text)));
        self.save_soon(cx);
        cx.notify();
    }

    /// Ends editing. An empty Text mark goes away.
    pub(super) fn finish_editing(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let empty = self.mark(editing.mark, cx).is_some_and(|m| m.kind == MarkKind::Text && m.text.trim().is_empty());
        if empty {
            let project = self.project_id;
            self.store.update(cx, |store, cx| {
                store.remove_mark(project, editing.mark);
                cx.notify();
            });
            if self.selected_mark == Some(editing.mark) {
                self.selected_mark = None;
            }
        }
        self.save_soon(cx);
        cx.notify();
    }

    /// Which line has the keyboard, and where its caret is.
    fn focused_line(&self, window: &Window, cx: &App) -> Option<(usize, usize)> {
        let editing = self.editing.as_ref()?;
        editing.lines.iter().enumerate().find_map(|(ix, line)| {
            line.focus_handle(cx).is_focused(window).then(|| (ix, line.read(cx).cursor()))
        })
    }

    fn focus_line(&self, index: usize, cursor: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(line) = self.editing.as_ref().and_then(|e| e.lines.get(index)).cloned() {
            line.update(cx, |input, cx| input.set_cursor(cursor, cx));
            window.focus(&line.focus_handle(cx));
        }
    }

    fn new_line(&mut self, _: &NewLine, window: &mut Window, cx: &mut Context<Self>) {
        let is_section = self.editing.as_ref().and_then(|e| self.mark(e.mark, cx)).is_some_and(|m| m.kind == MarkKind::Section);
        if is_section {
            self.finish_editing(cx);
            window.focus(&self.focus_handle);
            return;
        }
        let Some((index, cursor)) = self.focused_line(window, cx) else {
            return;
        };
        let Some(line) = self.editing.as_ref().map(|e| e.lines[index].clone()) else {
            return;
        };
        let text = line.read(cx).text().to_string();
        let (before, after) = text.split_at(cursor.min(text.len()));
        // A new line under a checkbox is a checkbox too.
        let carried = match checkbox(before) {
            Some(_) if !after.trim().is_empty() || !before.trim().is_empty() => "[ ] ",
            _ => "",
        };
        let (before, after) = (before.to_string(), format!("{carried}{after}"));
        line.update(cx, |input, cx| input.set_text(before, cx));
        let input = self.line_input(after, "", cx);
        let subscription = cx.subscribe(&input, Self::line_changed);
        if let Some(editing) = &mut self.editing {
            editing.lines.insert(index + 1, input);
            editing._subscriptions.push(subscription);
        }
        self.write_back(cx);
        self.focus_line(index + 1, carried.len(), window, cx);
    }

    fn line_up(&mut self, _: &LineUp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((index, cursor)) = self.focused_line(window, cx)
            && index > 0
        {
            self.focus_line(index - 1, cursor, window, cx);
        }
    }

    fn line_down(&mut self, _: &LineDown, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.editing.as_ref().map_or(0, |e| e.lines.len());
        if let Some((index, cursor)) = self.focused_line(window, cx)
            && index + 1 < count
        {
            self.focus_line(index + 1, cursor, window, cx);
        }
    }

    fn finish_editing_action(&mut self, _: &FinishEditing, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(cx);
        window.focus(&self.focus_handle);
    }

    /// Backspace at the start of a line joins it to the line above.
    fn join_line(&mut self, _: &text_input::Backspace, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, _)) = self.focused_line(window, cx).filter(|(index, _)| *index > 0) else {
            return;
        };
        let Some(editing) = &mut self.editing else {
            return;
        };
        let removed = editing.lines.remove(index);
        let text = removed.read(cx).text().to_string();
        let above = editing.lines[index - 1].clone();
        let cursor = above.read(cx).text().len();
        above.update(cx, |input, cx| {
            let joined = format!("{}{text}", input.text());
            input.set_text(joined, cx);
        });
        self.write_back(cx);
        self.focus_line(index - 1, cursor, window, cx);
    }

    /// Pasting several lines splits them into lines.
    fn paste_lines(&mut self, _: &text_input::Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pasted) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let Some((index, cursor)) = self.focused_line(window, cx) else {
            return;
        };
        let Some(line) = self.editing.as_ref().map(|e| e.lines[index].clone()) else {
            return;
        };
        let text = line.read(cx).text().to_string();
        let (before, after) = text.split_at(cursor.min(text.len()));
        let pasted = pasted.replace("\r\n", "\n");
        let mut parts: Vec<String> = pasted.split('\n').map(str::to_string).collect();
        let last_len = parts.last().map_or(0, String::len);
        parts[0] = format!("{before}{}", parts[0]);
        if let Some(last) = parts.last_mut() {
            last.push_str(after);
        }
        let count = parts.len();
        let mut parts = parts.into_iter();
        if let Some(first) = parts.next() {
            line.update(cx, |input, cx| input.set_text(first, cx));
        }
        for (offset, part) in parts.enumerate() {
            let input = self.line_input(part, "", cx);
            let subscription = cx.subscribe(&input, Self::line_changed);
            if let Some(editing) = &mut self.editing {
                editing.lines.insert(index + 1 + offset, input);
                editing._subscriptions.push(subscription);
            }
        }
        self.write_back(cx);
        self.focus_line(index + count - 1, last_len, window, cx);
    }

    // Gestures -------------------------------------------------------------------

    fn begin_mark_gesture(&mut self, id: u64, window: &Window, cx: &App) {
        let Some(mark) = self.mark(id, cx) else {
            return;
        };
        let viewport = self.viewport(cx);
        // A section carries the windows and marks inside it.
        let (windows, marks) = match (mark.kind, self.project(cx)) {
            (MarkKind::Section, Some(project)) => (
                project
                    .windows
                    .iter()
                    .filter(|f| mark.contains(f.x, f.y, f.width, f.height))
                    .map(|f| (f.item, f.x, f.y))
                    .collect(),
                project
                    .marks
                    .iter()
                    .filter(|m| m.id != id && mark.contains(m.x, m.y, m.width, m.height))
                    .map(|m| (m.id, m.x, m.y))
                    .collect(),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        self.mark_gesture = Some(MarkGesture {
            mouse: window.mouse_position(),
            zoom: viewport.zoom,
            start: mark,
            windows,
            marks,
        });
    }

    pub(super) fn drag_mark(&mut self, id: u64, edge: Option<Edge>, mouse: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(gesture) = self.mark_gesture.as_ref().filter(|g| g.start.id == id) else {
            return;
        };
        let start = &gesture.start;
        let dx = f32::from(mouse.x - gesture.mouse.x) / gesture.zoom;
        let dy = f32::from(mouse.y - gesture.mouse.y) / gesture.zoom;
        let project = self.project_id;
        match edge {
            None => {
                let x = settings::snap(start.x + dx, cx);
                let y = settings::snap(start.y + dy, cx);
                let (moved_x, moved_y) = (x - start.x, y - start.y);
                let (windows, marks) = (gesture.windows.clone(), gesture.marks.clone());
                let (width, height) = (start.width, start.height);
                self.store.update(cx, |store, _| {
                    store.update_mark(project, id, MarkChange::Bounds { x, y, width, height });
                    store.move_together(project, &windows, &marks, moved_x, moved_y);
                });
            }
            Some(edge) => {
                let frame = Frame {
                    item: id,
                    x: start.x,
                    y: start.y,
                    width: start.width,
                    height: start.height,
                };
                let next = resized_to_min(frame, edge, dx, dy, start.kind.min_size());
                let snap = |value| settings::snap(value, cx);
                let (x, y) = (snap(next.x), snap(next.y));
                let (right, bottom) = (snap(next.x + next.width), snap(next.y + next.height));
                self.store.update(cx, |store, _| {
                    store.update_mark(project, id, MarkChange::Bounds { x, y, width: right - x, height: bottom - y })
                });
            }
        }
        self.save_soon(cx);
        cx.notify();
    }

    /// The canvas point under `mouse` (window coordinates).
    fn canvas_point(&self, mouse: Point<Pixels>, cx: &App) -> (f32, f32) {
        let viewport = self.viewport(cx);
        let origin = self.bounds.get().origin;
        (
            viewport.x + f32::from(mouse.x - origin.x) / viewport.zoom,
            viewport.y + f32::from(mouse.y - origin.y) / viewport.zoom,
        )
    }

    /// Empty canvas was pressed with a tool that adds a mark.
    pub(super) fn press_with_tool(&mut self, kind: MarkKind, window: &mut Window, cx: &mut Context<Self>) {
        let mouse = window.mouse_position();
        if kind == MarkKind::Section {
            self.drawing = Some((mouse, mouse));
            cx.notify();
            return;
        }
        let (x, y) = self.canvas_point(mouse, cx);
        self.add_mark_at(kind, x, y, None, window, cx);
    }

    /// The pointer went up anywhere: ends a section being drawn, and mark
    /// drags.
    pub(super) fn mouse_released(&mut self, _: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.mark_gesture = None;
        let Some((from, to)) = self.drawing.take() else {
            return;
        };
        let (x0, y0) = self.canvas_point(from, cx);
        let (x1, y1) = self.canvas_point(to, cx);
        let zoom = self.viewport(cx).zoom;
        let (min_w, min_h) = MarkKind::Section.min_size();
        // A click (or a tiny drag) makes one of the default size.
        let size = ((x1 - x0).abs() * zoom > 24. || (y1 - y0).abs() * zoom > 24.)
            .then(|| ((x1 - x0).abs().max(min_w), (y1 - y0).abs().max(min_h)));
        self.add_mark_at(MarkKind::Section, x0.min(x1), y0.min(y1), size, window, cx);
    }

    // Rendering ------------------------------------------------------------------

    /// Where `mark` is on screen. A Text mark is as big as its text.
    fn place(&self, mark: &Mark, viewport: Viewport, window: &Window, cx: &App) -> Placed {
        let (width, height) = match mark.kind {
            MarkKind::Text => self.text_size(mark, window, cx),
            _ => (mark.width, mark.height),
        };
        Placed {
            left: (mark.x - viewport.x) * viewport.zoom,
            top: (mark.y - viewport.y) * viewport.zoom,
            width: width * viewport.zoom,
            height: height * viewport.zoom,
        }
    }

    /// A Text mark's size at 100%: its longest line by its lines.
    fn text_size(&self, mark: &Mark, window: &Window, _: &App) -> (f32, f32) {
        let font_size = px(mark.text_size);
        let mut font = window.text_style().font();
        font.weight = FontWeight::SEMIBOLD;
        let lines: Vec<&str> = mark.text.split('\n').collect();
        let widest = lines
            .iter()
            .map(|line| {
                let line = if line.is_empty() { " " } else { line };
                let run = TextRun {
                    len: line.len(),
                    font: font.clone(),
                    color: gpui::black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                f32::from(window.text_system().shape_line(line.to_string().into(), font_size, &[run], None).width)
            })
            .fold(0., f32::max);
        let editing = self.editing.as_ref().is_some_and(|e| e.mark == mark.id);
        // Room for the caret (and the placeholder) while typing.
        let widest = if editing { widest.max(mark.text_size * 6.) + mark.text_size } else { widest.max(mark.text_size) };
        (
            widest + TEXT_PADDING.0 * 2.,
            lines.len() as f32 * (mark.text_size * 1.25).round() + TEXT_PADDING.1 * 2.,
        )
    }

    /// Sections, behind the windows.
    pub(super) fn render_sections(&self, project: &Project, viewport: Viewport, theme: &Theme, window: &Window, cx: &Context<Self>) -> Vec<AnyElement> {
        project
            .marks
            .iter()
            .filter(|m| m.kind.behind_windows())
            .map(|mark| self.render_mark(mark, viewport, theme, window, cx))
            .collect()
    }

    /// Text and notes, in front of the windows.
    pub(super) fn render_front_marks(&self, project: &Project, viewport: Viewport, theme: &Theme, window: &Window, cx: &Context<Self>) -> Vec<AnyElement> {
        project
            .marks
            .iter()
            .filter(|m| !m.kind.behind_windows())
            .map(|mark| self.render_mark(mark, viewport, theme, window, cx))
            .collect()
    }

    fn render_mark(&self, mark: &Mark, viewport: Viewport, theme: &Theme, window: &Window, cx: &Context<Self>) -> AnyElement {
        let id = mark.id;
        let placed = self.place(mark, viewport, window, cx);
        let zoom = viewport.zoom;
        let (width, height) = (placed.width / zoom, placed.height / zoom);
        let editing = self.editing.as_ref().filter(|e| e.mark == id);
        let title = if mark.kind == MarkKind::Section { SECTION_TITLE_HEIGHT } else { 0. };
        let contents = match mark.kind {
            MarkKind::Text => self.render_text(mark, editing, theme, cx).into_any_element(),
            MarkKind::Note => self.render_note(mark, editing, (width, height), theme, cx).into_any_element(),
            MarkKind::Section => self.render_section(mark, editing, (width, height), theme, cx).into_any_element(),
        };
        let grabbable = editing.is_none();
        div()
            .id(ElementId::named_usize("mark", id as usize))
            .absolute()
            .left(px(placed.left))
            .top(px(placed.top - title * zoom))
            .w(px(placed.width))
            .h(px(placed.height + title * zoom))
            // A section's body lets clicks through to the canvas (to pan);
            // its name and the other marks take them.
            .when(mark.kind != MarkKind::Section, |el| {
                el.occlude()
                    .child(layers::occluder())
                    .when(grabbable, |el| el.cursor(CursorStyle::OpenHand))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            window.prevent_default();
                            if this.editing.as_ref().is_some_and(|e| e.mark == id) {
                                return;
                            }
                            if event.click_count >= 2 {
                                this.start_editing(id, window, cx);
                                return;
                            }
                            this.select_mark(id, window, cx);
                            this.begin_mark_gesture(id, window, cx);
                        }),
                    )
                    .when(grabbable, |el| {
                        el.on_drag(MoveMark(id), |_, _, window, cx| {
                            cx.set_active_drag_cursor_style(CursorStyle::ClosedHand, window);
                            cx.new(|_| NoPreview)
                        })
                    })
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.select_mark(id, window, cx);
                    this.open_mark_menu(id, window, cx);
                }),
            )
            .child(Zoomed {
                size: (width, height + title),
                zoom,
                child: div()
                    .flex_none()
                    .w(px(width))
                    .h(px(height + title))
                    .child(contents)
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn editor_container(&self, editing: &MarkEditing, cx: &Context<Self>) -> gpui::Div {
        div()
            .key_context(EDITOR_CONTEXT)
            .on_action(cx.listener(Self::new_line))
            .on_action(cx.listener(Self::line_up))
            .on_action(cx.listener(Self::line_down))
            .on_action(cx.listener(Self::finish_editing_action))
            .on_action(cx.listener(Self::join_line))
            .on_action(cx.listener(Self::paste_lines))
            .flex()
            .flex_col()
            .children(editing.lines.iter().map(|line| div().w_full().child(line.clone())))
    }

    fn render_text(&self, mark: &Mark, editing: Option<&MarkEditing>, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let color = tint(mark.color, theme).map_or(theme.foreground, |tint| tint.base);
        let line_height = (mark.text_size * 1.25).round();
        div()
            .size_full()
            .px(px(TEXT_PADDING.0))
            .py(px(TEXT_PADDING.1))
            .text_size(px(mark.text_size))
            .line_height(px(line_height))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(color)
            .whitespace_nowrap()
            .map(|el| match editing {
                Some(editing) => el.child(self.editor_container(editing, cx)),
                None => el.children(mark.text.split('\n').map(|line| div().h(px(line_height)).child(line.to_string()))),
            })
    }

    fn render_note(&self, mark: &Mark, editing: Option<&MarkEditing>, (width, height): (f32, f32), theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let tint = tint(mark.color, theme);
        let (bg, fg, border) = match tint {
            Some(tint) => (tint.soft, tint.soft_foreground, tint.base.opacity(0.35)),
            None => (theme.muted, theme.foreground, theme.border),
        };
        let id = mark.id;
        let muted = fg.opacity(0.55);
        div()
            .w(px(width))
            .h(px(height))
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(14.))
            .overflow_hidden()
            .rounded(px(12.))
            .border_1()
            .border_color(border)
            .bg(bg)
            .shadow(vec![gpui::BoxShadow {
                color: gpui::black().opacity(0.10),
                offset: point(px(0.), px(4.)),
                blur_radius: px(12.),
                spread_radius: px(-2.),
            }])
            .text_size(px(14.))
            .line_height(px(20.))
            .text_color(fg)
            .map(|el| match editing {
                Some(editing) => el.child(self.editor_container(editing, cx)),
                None if mark.text.trim().is_empty() => el.child(div().text_color(muted).child(t("Double-click to write"))),
                None => el.children(mark.text.split('\n').enumerate().map(|(ix, line)| match checkbox(line) {
                    Some((done, text)) => div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(
                            div()
                                .id(ElementId::NamedInteger("mark-check".into(), id * 1000 + ix as u64))
                                .mt(px(2.))
                                .cursor_pointer()
                                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                    window.prevent_default();
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(mark) = this.mark(id, cx) {
                                        this.change_mark(id, MarkChange::Text(toggle_checkbox(&mark.text, ix)), cx);
                                    }
                                }))
                                .child(ui::icon(if done { IconName::SquareCheck } else { IconName::Square }).text_color(if done { fg } else { muted })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .when(done, |el| el.line_through().text_color(muted))
                                .child(text.to_string()),
                        )
                        .into_any_element(),
                    None if line.is_empty() => div().h(px(20.)).into_any_element(),
                    None => div().child(line.to_string()).into_any_element(),
                })),
            })
    }

    fn render_section(&self, mark: &Mark, editing: Option<&MarkEditing>, (width, height): (f32, f32), theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let accent = tint(mark.color, theme).map_or(theme.muted_foreground, |tint| tint.base);
        let id = mark.id;
        let selected = self.selected_mark == Some(id);
        div()
            .w(px(width))
            .h(px(height + SECTION_TITLE_HEIGHT))
            .flex()
            .flex_col()
            .child(
                // The name: the handle that moves the section.
                div().h(px(SECTION_TITLE_HEIGHT)).flex().items_start().child(
                    div()
                        .id(ElementId::named_usize("section-title", id as usize))
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .h(px(22.))
                        .px(px(8.))
                        .max_w(px(width))
                        .rounded(px(7.))
                        .bg(accent)
                        .text_color(gpui::white())
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .occlude()
                        .cursor(CursorStyle::OpenHand)
                        .child(ui::icon(IconName::Frame).size_3().text_color(gpui::white().opacity(0.85)))
                        .map(|el| match editing {
                            Some(editing) => el.min_w(px(120.)).child(self.editor_container(editing, cx)),
                            None => el.child(div().truncate().child(if mark.text.is_empty() { t("Section").to_string() } else { mark.text.clone() })),
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                window.prevent_default();
                                if this.editing.as_ref().is_some_and(|e| e.mark == id) {
                                    return;
                                }
                                if event.click_count >= 2 {
                                    this.start_editing(id, window, cx);
                                    return;
                                }
                                this.select_mark(id, window, cx);
                                this.begin_mark_gesture(id, window, cx);
                            }),
                        )
                        .when(editing.is_none(), |el| {
                            el.on_drag(MoveMark(id), |_, _, window, cx| {
                                cx.set_active_drag_cursor_style(CursorStyle::ClosedHand, window);
                                cx.new(|_| NoPreview)
                            })
                        }),
                ),
            )
            .child(
                // The area: a faint wash of its color.
                div()
                    .flex_1()
                    .rounded(px(16.))
                    .border(px(if selected { 2. } else { 1.5 }))
                    .border_color(accent.opacity(if selected { 0.7 } else { 0.35 }))
                    .bg(accent.opacity(0.05)),
            )
    }

    /// The selected mark's outline, resize handles and toolbar, in screen
    /// px over everything else.
    pub(super) fn render_selection(&self, project: &Project, viewport: Viewport, theme: &Theme, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let id = self.selected_mark?;
        let mark = project.marks.iter().find(|m| m.id == id)?;
        let placed = self.place(mark, viewport, window, cx);
        let accent = theme.tints.blue.base;
        let resizable = mark.kind != MarkKind::Text;
        let outline = div()
            .absolute()
            .left(px(placed.left - 2.))
            .top(px(placed.top - 2.))
            .w(px(placed.width + 4.))
            .h(px(placed.height + 4.))
            .rounded(px(if mark.kind == MarkKind::Text { 4. } else { 14. * viewport.zoom + 2. }))
            .border_1()
            .border_color(accent)
            .when(resizable, |el| {
                el.children(Edge::ALL.into_iter().map(|edge| {
                    resize_handle(edge)
                        .id(ElementId::named_usize("mark-resize", id as usize * 8 + edge as usize))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                window.prevent_default();
                                this.begin_mark_gesture(id, window, cx)
                            }),
                        )
                        .on_drag(ResizeMark { mark: id, edge }, move |_, _, window, cx| {
                            cx.set_active_drag_cursor_style(edge.cursor(), window);
                            cx.new(|_| NoPreview)
                        })
                }))
                .children(
                    [(true, true), (true, false), (false, true), (false, false)].into_iter().map(|(left, top)| {
                        div()
                            .absolute()
                            .size(px(8.))
                            .rounded(px(2.))
                            .border_1()
                            .border_color(accent)
                            .bg(theme.background)
                            .map(|el| if left { el.left(px(-4.)) } else { el.right(px(-4.)) })
                            .map(|el| if top { el.top(px(-4.)) } else { el.bottom(px(-4.)) })
                    }),
                )
            });
        let title = if mark.kind == MarkKind::Section { SECTION_TITLE_HEIGHT * viewport.zoom } else { 0. };
        let toolbar_top = placed.top - title - 46.;
        let toolbar_top = if toolbar_top < 8. { placed.top + placed.height + 10. } else { toolbar_top };
        let toolbar = (self.editing.is_none()).then(|| self.render_mark_toolbar(mark, theme, cx).left(px(placed.left)).top(px(toolbar_top)));
        Some(div().absolute().top_0().left_0().size_full().child(outline).children(toolbar).into_any_element())
    }

    fn render_mark_toolbar(&self, mark: &Mark, theme: &Theme, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let id = mark.id;
        let kind = mark.kind;
        floating_bar("mark-toolbar", theme)
            .children(ProjectColor::ALL.into_iter().map(|color| {
                let fill = color.resolve(&theme.tints, theme.muted_foreground);
                let selected = mark.color == color;
                div()
                    .id(ElementId::NamedInteger("mark-color".into(), color as u64))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(26.))
                    .rounded(radius::SM)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.secondary))
                    .child(
                        div()
                            .size(px(14.))
                            .rounded_full()
                            .bg(fill)
                            .when(selected, |el| el.border_2().border_color(theme.foreground)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.change_mark(id, MarkChange::Color(color), cx)))
            }))
            .when(kind == MarkKind::Text, |bar| {
                bar.child(toolbar_divider(theme)).children(TEXT_SIZES.into_iter().zip(["S", "M", "L", "XL"]).map(|(size, label)| {
                    let selected = mark.text_size == size;
                    div()
                        .id(SharedString::from(format!("mark-size-{label}")))
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(26.))
                        .min_w(px(26.))
                        .px(px(6.))
                        .rounded(radius::SM)
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .text_color(if selected { theme.foreground } else { theme.muted_foreground })
                        .when(selected, |el| el.bg(theme.secondary))
                        .hover(|style| style.bg(theme.secondary))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| this.change_mark(id, MarkChange::TextSize(size), cx)))
                }))
            })
            .child(toolbar_divider(theme))
            .child(
                ui::small_icon_button("mark-edit", IconName::Pencil, theme)
                    .on_click(cx.listener(move |this, _, window, cx| this.start_editing(id, window, cx))),
            )
            .child(
                ui::small_icon_button("mark-duplicate", IconName::CopyPlus, theme)
                    .on_click(cx.listener(move |this, _, window, cx| this.duplicate_mark(id, window, cx))),
            )
            .child(
                ui::small_icon_button("mark-delete", IconName::Trash, theme).on_click(cx.listener(move |this, _, _, cx| {
                    this.selected_mark = Some(id);
                    this.delete_selected_mark(cx);
                })),
            )
    }

    fn open_mark_menu(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let canvas = cx.entity().downgrade();
        let action = |f: fn(&mut Self, u64, &mut Window, &mut Context<Self>)| {
            let canvas = canvas.clone();
            move |window: &mut Window, cx: &mut App| {
                canvas.update(cx, |this, cx| f(this, id, window, cx)).ok();
            }
        };
        let entries = vec![
            MenuEntry::item(IconName::Pencil, t("Edit text"), action(Self::start_editing)),
            MenuEntry::item(IconName::CopyPlus, t("Duplicate"), action(Self::duplicate_mark)),
            MenuEntry::Separator,
            MenuEntry::destructive(IconName::Trash, t("Delete"), action(|this, id, _, cx| {
                this.selected_mark = Some(id);
                this.delete_selected_mark(cx);
            })),
        ];
        cx.emit(CanvasEvent::Menu {
            item: id,
            entries,
            position: window.mouse_position().into(),
        });
    }

    /// Right-clicking empty canvas: add something where you clicked.
    pub(super) fn open_canvas_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mouse = window.mouse_position();
        let (x, y) = self.canvas_point(mouse, cx);
        let canvas = cx.entity().downgrade();
        let add = |kind: MarkKind| {
            let canvas = canvas.clone();
            move |window: &mut Window, cx: &mut App| {
                canvas.update(cx, |this, cx| this.add_mark_at(kind, x, y, None, window, cx)).ok();
            }
        };
        let mut entries = vec![
            MenuEntry::item(IconName::Type, t("Add text here"), add(MarkKind::Text)),
            MenuEntry::item(IconName::StickyNote, t("Add a note here"), add(MarkKind::Note)),
            MenuEntry::item(IconName::Frame, t("Add a section here"), add(MarkKind::Section)),
            MenuEntry::Separator,
        ];
        if Mods::installed(ItemKind::Terminal, cx) {
            let canvas = canvas.clone();
            entries.push(MenuEntry::item(IconName::SquareTerminal, t("New terminal here"), move |window, cx| {
                canvas
                    .update(cx, |this, cx| this.add_window_at(ItemKind::Terminal, x, y, window, cx))
                    .ok();
            }));
        }
        let tidy = canvas.clone();
        entries.push(MenuEntry::item(IconName::LayoutGrid, t("Tidy up windows"), move |_, cx| {
            tidy.update(cx, |this, cx| this.tidy(cx)).ok();
        }));
        let fit = canvas.clone();
        entries.push(MenuEntry::item(IconName::Maximize, t("Zoom to fit"), move |window, cx| {
            fit.update(cx, |this, cx| this.run(CanvasCommand::ZoomToFit, window, cx)).ok();
        }));
        cx.emit(CanvasEvent::Menu {
            item: 0,
            entries,
            position: mouse.into(),
        });
    }

    /// The rectangle of a section being drawn.
    pub(super) fn render_drawing(&self, theme: &Theme) -> Option<AnyElement> {
        let (from, to) = self.drawing?;
        let origin = self.bounds.get().origin;
        let (left, top) = (f32::from(from.x.min(to.x) - origin.x), f32::from(from.y.min(to.y) - origin.y));
        let (width, height) = (f32::from((to.x - from.x).abs()), f32::from((to.y - from.y).abs()));
        let accent = theme.tints.blue.base;
        Some(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(width))
                .h(px(height))
                .rounded(px(12.))
                .border_1()
                .border_color(accent)
                .bg(accent.opacity(0.06))
                .into_any_element(),
        )
    }

    /// The canvas's tools, bottom centre: select, hand, text, note,
    /// section, and new windows.
    pub(super) fn render_toolbar(&self, theme: &Theme, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let tool = self.tool;
        let button = |id: &'static str, icon: IconName, selected: bool, tooltip: &'static str, keys: &'static str| {
            let (bg, fg, hover) = (theme.primary, theme.primary_foreground, theme.secondary);
            let muted = theme.muted_foreground;
            let foreground = theme.foreground;
            div()
                .id(id)
                .group(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(32.))
                .rounded(radius::MD)
                .cursor_pointer()
                .map(|el| if selected { el.bg(bg) } else { el.hover(move |style| style.bg(hover)) })
                .child(ui::icon(icon).text_color(if selected { fg } else { muted }).group_hover(id, move |s| s.text_color(foreground)))
                .tooltip(move |_, cx| cx.new(|_| ui::Tooltip::new(t(tooltip), keys)).into())
        };
        floating_bar("canvas-toolbar", theme)
            .gap(px(2.))
            .p(px(4.))
            .child(button("tool-select", IconName::MousePointer, tool == Tool::Select, "Select", "V").on_click(cx.listener(|this, _, window, cx| this.set_tool(Tool::Select, window, cx))))
            .child(button("tool-hand", IconName::Hand, tool == Tool::Hand, "Hand: drag to pan", "H").on_click(cx.listener(|this, _, window, cx| this.set_tool(Tool::Hand, window, cx))))
            .child(toolbar_divider(theme))
            .child(button("tool-text", IconName::Type, tool == Tool::Add(MarkKind::Text), "Text", "T").on_click(cx.listener(|this, _, window, cx| this.set_tool(Tool::Add(MarkKind::Text), window, cx))))
            .child(button("tool-note", IconName::StickyNote, tool == Tool::Add(MarkKind::Note), "Note", "N").on_click(cx.listener(|this, _, window, cx| this.set_tool(Tool::Add(MarkKind::Note), window, cx))))
            .child(button("tool-section", IconName::Frame, tool == Tool::Add(MarkKind::Section), "Section", "S").on_click(cx.listener(|this, _, window, cx| this.set_tool(Tool::Add(MarkKind::Section), window, cx))))
            .child(toolbar_divider(theme))
            .child(button("tool-tidy", IconName::LayoutGrid, false, "Tidy up windows", "").on_click(cx.listener(|this, _, _, cx| this.tidy(cx))))
            .child(
                // The toolbar is at the bottom: the menu opens over it.
                button("tool-new-window", IconName::Plus, false, "New window", "")
                    .child(self.anchors.track("new-window"))
                    .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                        let at = this.anchors.above("new-window", ui::Align::Start, event.position());
                        cx.emit(CanvasEvent::NewWindowMenu(at))
                    })),
            )
    }
}

/// A floating pill over the canvas: border bg-popover rounded-xl shadow.
fn floating_bar(id: &'static str, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .absolute()
        .flex()
        .items_center()
        .gap(px(2.))
        .p(px(4.))
        .rounded(radius::LG)
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .text_color(theme.popover_foreground)
        .shadow(vec![gpui::BoxShadow {
            color: gpui::black().opacity(0.12),
            offset: point(px(0.), px(6.)),
            blur_radius: px(18.),
            spread_radius: px(-4.),
        }])
        .block_mouse_except_scroll()
        // Using it leaves the keyboard where it was.
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .child(layers::occluder())
}

fn toolbar_divider(theme: &Theme) -> gpui::Div {
    div().w_px().h(px(18.)).mx(px(3.)).bg(theme.border)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkboxes_are_lines_starting_with_brackets() {
        assert_eq!(checkbox("[ ] milk"), Some((false, "milk")));
        assert_eq!(checkbox("  [x] eggs"), Some((true, "eggs")));
        assert_eq!(checkbox("- [ ] bread"), Some((false, "bread")));
        assert_eq!(checkbox("plain"), None);
    }

    #[test]
    fn ticking_a_checkbox_changes_only_its_line() {
        let text = "todo\n[ ] one\n[x] two";
        assert_eq!(toggle_checkbox(text, 1), "todo\n[x] one\n[x] two");
        assert_eq!(toggle_checkbox(text, 2), "todo\n[ ] one\n[ ] two");
        assert_eq!(toggle_checkbox(text, 0), text);
    }
}
