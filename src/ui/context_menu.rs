//! A menu of commands, styled as the Astralyx `ContextMenu` / `DropdownMenu`.
//!
//! The menu only renders its surface; the owner places it (see
//! `deferred(anchored()...)` in main.rs) and drops it on [`DismissEvent`],
//! which fires after an item runs, on Escape, or on a click outside.
//! Up/Down move the highlight, Enter runs it.

use std::rc::Rc;

use gpui::{
    App, Context, DismissEvent, ElementId, EventEmitter, FocusHandle, Focusable, KeyBinding,
    SharedString, Window, div, prelude::*, px,
};

use crate::ui::actions::{Cancel, Confirm, SelectNext, SelectPrevious};
use crate::ui::assets::IconName;
use crate::ui::theme::{Theme, radius};
use crate::ui;

const KEY_CONTEXT: &str = "ContextMenu";

/// Where a menu opens: a point, and which of the menu's corners sits on it.
/// A right-click menu opens down-right from the pointer (a plain point
/// converts to that); a dropdown opens against its button (see
/// [`ui::Anchors`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuAt {
    pub point: gpui::Point<gpui::Pixels>,
    pub corner: gpui::Corner,
}

impl From<gpui::Point<gpui::Pixels>> for MenuAt {
    fn from(point: gpui::Point<gpui::Pixels>) -> Self {
        MenuAt { point, corner: gpui::Corner::TopLeft }
    }
}

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("enter", Confirm, context),
        KeyBinding::new("escape", Cancel, context),
    ]);
}

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Clone)]
pub enum MenuEntry {
    Item {
        /// `None` leaves the icon's space empty (an unchecked check item).
        icon: Option<IconName>,
        label: SharedString,
        destructive: bool,
        handler: Handler,
    },
    Separator,
    /// A heading that isn't picked: a title, a line under it, and
    /// optionally an avatar's initials and color (the profile menu).
    Header {
        title: SharedString,
        subtitle: SharedString,
        avatar: Option<(SharedString, gpui::Hsla)>,
    },
}

impl MenuEntry {
    pub fn item(
        icon: IconName,
        label: impl Into<SharedString>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        MenuEntry::Item {
            icon: Some(icon),
            label: label.into(),
            destructive: false,
            handler: Rc::new(handler),
        }
    }

    /// An on/off setting, with a check mark while it's on.
    pub fn check(
        label: impl Into<SharedString>,
        checked: bool,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        MenuEntry::Item {
            icon: checked.then_some(IconName::Check),
            label: label.into(),
            destructive: false,
            handler: Rc::new(handler),
        }
    }

    /// An item in the destructive color (e.g. Delete).
    pub fn destructive(
        icon: IconName,
        label: impl Into<SharedString>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        MenuEntry::Item {
            icon: Some(icon),
            label: label.into(),
            destructive: true,
            handler: Rc::new(handler),
        }
    }

    fn is_item(&self) -> bool {
        matches!(self, MenuEntry::Item { .. })
    }
}

pub struct ContextMenu {
    entries: Vec<MenuEntry>,
    highlighted: Option<usize>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for ContextMenu {}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ContextMenu {
    /// Creates the menu and focuses it, so the keyboard drives it.
    pub fn new(entries: Vec<MenuEntry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self {
            entries,
            highlighted: None,
            focus_handle,
        }
    }

    fn run(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(MenuEntry::Item { handler, .. }) = self.entries.get(ix) {
            let handler = handler.clone();
            handler(window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.highlighted {
            self.run(ix, window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let start = self.highlighted.map_or(0, |ix| ix + 1);
        let next = (start..self.entries.len())
            .chain(0..start)
            .find(|&ix| self.entries[ix].is_item());
        self.highlighted = next;
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.entries.len();
        let start = self.highlighted.unwrap_or(0);
        let previous = (0..start)
            .rev()
            .chain((start..len).rev())
            .find(|&ix| self.entries[ix].is_item());
        self.highlighted = previous;
        cx.notify();
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);

        // menuSurface: border bg-popover text-popover-foreground p-1,
        // radius.surface (rounded-2xl).
        div()
            .id("context-menu")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .flex()
            .flex_col()
            .min_w(px(200.))
            .p_1()
            .rounded(radius::XL2)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .shadow(vec![gpui::BoxShadow {
                color: gpui::black().opacity(0.16),
                offset: gpui::point(px(0.), px(8.)),
                blur_radius: px(24.),
                spread_radius: px(-6.),
            }])
            .children(self.entries.iter().enumerate().map(|(ix, entry)| {
                match entry {
                    // bg-border -mx-1 my-1 h-px
                    MenuEntry::Separator => div()
                        .h_px()
                        .my_1()
                        .mx(px(-4.))
                        .bg(theme.border)
                        .into_any_element(),
                    MenuEntry::Header { title, subtitle, avatar } => div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px(px(10.))
                        .py(px(8.))
                        .children(avatar.as_ref().map(|(initials, color)| ui::avatar(initials.clone(), *color, 32.)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(px(14.))
                                        .line_height(px(20.))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(title.clone()),
                                )
                                .when(!subtitle.is_empty(), |el| {
                                    el.child(
                                        div()
                                            .text_size(px(12.))
                                            .line_height(px(16.))
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(subtitle.clone()),
                                    )
                                }),
                        )
                        .into_any_element(),
                    MenuEntry::Item {
                        icon,
                        label,
                        destructive,
                        ..
                    } => {
                        let highlighted = self.highlighted == Some(ix);
                        // Destructive items keep the soft destructive ink and
                        // highlight with a 12% destructive wash.
                        let (fg, highlight_fg, highlight_bg) = if *destructive {
                            let tint = theme.tints.destructive;
                            (tint.soft_foreground, tint.soft_foreground, tint.base.opacity(0.12))
                        } else {
                            (theme.popover_foreground, theme.accent_foreground, theme.accent)
                        };
                        let fg = if highlighted { highlight_fg } else { fg };

                        // menuItem: gap-2 px-2.5 py-1.5 text-sm, rounded-lg,
                        // bg-accent while highlighted.
                        div()
                            .id(ElementId::named_usize("menu-item", ix))
                            .flex()
                            .items_center()
                            .gap_2()
                            .w_full()
                            .px(px(10.))
                            .py(px(6.))
                            .rounded(radius::LG)
                            .text_size(px(14.))
                            .line_height(px(20.))
                            .text_color(fg)
                            .when(highlighted, |item| item.bg(highlight_bg))
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if *hovered {
                                    this.highlighted = Some(ix);
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run(ix, window, cx);
                            }))
                            .child(match icon {
                                Some(icon) => ui::icon(*icon).text_color(fg).into_any_element(),
                                // Keeps labels lined up with checked items.
                                None => div().size_4().flex_shrink_0().into_any_element(),
                            })
                            .child(div().flex_1().whitespace_nowrap().child(label.clone()))
                            .into_any_element()
                    }
                }
            }))
    }
}
