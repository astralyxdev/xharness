//! Astralyx UI components rebuilt with GPUI elements. Sizes and states follow
//! the classes on ui.astralyx.dev; each builder notes the classes it mirrors.

pub mod actions;
pub mod alert_dialog;
pub mod assets;
pub mod context_menu;
pub mod text_input;
pub mod theme;

use std::f32::consts::FRAC_PI_2;

use gpui::{
    App, ClickEvent, Div, ElementId, FontWeight, Hsla, SharedString, Stateful, Svg,
    Transformation, Window, div, prelude::*, px, radians, rems, svg,
};

use crate::ui::assets::IconName;
use crate::ui::theme::{Theme, radius};

/// A 16px (`size-4`) icon. GPUI icons don't inherit text color, so callers
/// set it with `text_color` / `group_hover`.
pub fn icon(name: IconName) -> Svg {
    svg().path(name.path()).size_4().flex_shrink_0()
}

#[derive(Clone, Copy)]
pub enum Variant {
    /// Solid `--primary` button ("Get started" on the site).
    Primary,
    /// Transparent button that fills with `--secondary` on hover ("Browse all").
    Ghost,
    /// Filled `--secondary` button (the alert dialog's Cancel).
    Soft,
    /// Solid `--destructive` button (the alert dialog's Delete).
    Destructive,
}

#[derive(Clone, Copy)]
pub enum ButtonSize {
    /// h-9 px-4.5 rounded-[var(--radius-control-md)] (dialogs, forms)
    Default,
    /// h-10 px-6 rounded-[var(--radius-control-lg)] (hero actions)
    Large,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Astralyx `Button`: text-xs font-semibold, with hover and press fills.
/// Disabled buttons are `opacity-50` and ignore clicks.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    icon: Option<IconName>,
    variant: Variant,
    size: ButtonSize,
    disabled: bool,
    on_click: Option<ClickHandler>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            variant: Variant::Primary,
            size: ButtonSize::Default,
            disabled: false,
            on_click: None,
        }
    }

    /// A leading icon. Like the site's `has-[>svg]` rule, the button's
    /// padding tightens to make room for it.
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn variant(mut self, variant: Variant) -> Self {
        self.variant = variant;
        self
    }

    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::global(cx);
        let (bg, fg, hover, active) = match self.variant {
            Variant::Primary => (
                Some(theme.primary),
                theme.primary_foreground,
                theme.primary_hover,
                theme.primary_active,
            ),
            Variant::Ghost => (
                None,
                theme.secondary_foreground,
                theme.secondary,
                theme.secondary_active,
            ),
            Variant::Soft => (
                Some(theme.secondary),
                theme.secondary_foreground,
                theme.secondary_hover,
                theme.secondary_active,
            ),
            Variant::Destructive => (
                Some(theme.tints.destructive.base),
                theme.tints.destructive.foreground,
                theme.tints.destructive.hover,
                theme.destructive_active,
            ),
        };
        let with_icon = self.icon.is_some();
        let (height, padding_x, gap, corner) = match self.size {
            ButtonSize::Default => (px(36.), if with_icon { 14. } else { 18. }, px(8.), radius::CONTROL_MD),
            ButtonSize::Large => (px(40.), if with_icon { 20. } else { 24. }, px(8.), radius::CONTROL_LG),
        };

        div()
            .id(self.id)
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .gap(gap)
            .h(height)
            .px(px(padding_x))
            .rounded(corner)
            .text_size(px(12.))
            .line_height(px(16.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(fg)
            .when_some(bg, |el, bg| el.bg(bg))
            .map(|el| {
                if self.disabled {
                    el.opacity(0.5)
                } else {
                    el.hover(move |style| style.bg(hover))
                        .active(move |style| style.bg(active))
                        .when_some(self.on_click, |el, handler| el.on_click(handler))
                }
            })
            .when_some(self.icon, |el, name| el.child(icon(name).text_color(fg)))
            .child(self.label)
    }
}

/// A compact button for inside canvas windows, sized in rems so it zooms
/// with the canvas: h-7 px-2.5 text-xs font-semibold, soft or primary.
pub fn compact_button(
    id: impl Into<ElementId>,
    icon: Option<IconName>,
    label: &str,
    primary: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let (bg, fg, hover, active) = if primary {
        (theme.primary, theme.primary_foreground, theme.primary_hover, theme.primary_active)
    } else {
        (theme.secondary, theme.secondary_foreground, theme.secondary_hover, theme.secondary_active)
    };
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_1p5()
        .h_7()
        .px_2p5()
        .rounded(rems(0.5))
        .text_size(rems(0.75))
        .line_height(rems(1.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(fg)
        .bg(bg)
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(active))
        .children(icon.map(|name| self::icon(name).size_3p5().text_color(fg)))
        .child(label.to_string())
}

/// Compact ghost icon button for window headers: size-6, 14px muted icon,
/// `--secondary` hover fill.
pub fn small_icon_button(id: impl Into<ElementId>, name: IconName, theme: &Theme) -> Stateful<Div> {
    let (hover_bg, active_bg) = (theme.secondary, theme.secondary_active);
    let (fg, hover_fg) = (theme.muted_foreground, theme.foreground);

    div()
        .id(id)
        .group("small-icon-button")
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size_6()
        .rounded(radius::SM)
        .hover(move |style| style.bg(hover_bg))
        .active(move |style| style.bg(active_bg))
        .child(
            icon(name)
                .size_3p5()
                .text_color(fg)
                .group_hover("small-icon-button", move |style| style.text_color(hover_fg)),
        )
}

/// A 6px status dot (e.g. green for a running window).
pub fn status_dot(color: Hsla) -> Div {
    div().flex_shrink_0().size(px(6.)).rounded_full().bg(color)
}

/// `SidebarMenuButton`: h-9 w-full px-2.5 gap-2 text-sm font-medium
/// rounded-lg.
///
/// - active:   bg sidebar-foreground/15, text sidebar-foreground
/// - inactive: text sidebar-foreground/70, hover bg /10 + full text color
///
/// `icon_only` drops the label, for a caller that puts its own content
/// there (inline rename).
/// `icon_color` pins the icon to a color (project icons); otherwise it
/// follows the text. Callers can append trailing children (e.g. a chevron).
pub fn sidebar_menu_button(
    id: impl Into<ElementId>,
    name: IconName,
    label: impl Into<SharedString>,
    active: bool,
    icon_only: bool,
    icon_color: Option<Hsla>,
    theme: &Theme,
) -> Stateful<Div> {
    let fg = theme.sidebar_foreground;
    let rest_fg = if active { fg } else { fg.opacity(0.7) };

    div()
        .id(id)
        .group("sidebar-menu-button")
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        .w_full()
        .h_9()
        .px(px(10.))
        .overflow_hidden()
        .rounded(radius::LG)
        .text_size(px(14.))
        .line_height(px(20.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rest_fg)
        .map(|el| {
            if active {
                el.bg(fg.opacity(0.15))
            } else {
                el.hover(move |style| style.bg(fg.opacity(0.1)).text_color(fg))
            }
        })
        .child(match icon_color {
            Some(color) => icon(name).text_color(color),
            None => icon(name)
                .text_color(rest_fg)
                .group_hover("sidebar-menu-button", move |style| style.text_color(fg)),
        })
        .when(!icon_only, |el| {
            el.child(div().flex_1().min_w_0().truncate().child(label.into()))
        })
}

/// Disclosure chevron for collapsible rows: Lucide `chevron-right`, turned
/// 90° when open (the Tree component's `rotate-90`).
pub fn chevron(open: bool, theme: &Theme) -> Svg {
    let fg = theme.sidebar_foreground;
    icon(IconName::ChevronRight)
        .text_color(fg.opacity(0.5))
        .group_hover("sidebar-menu-button", move |style| style.text_color(fg))
        .when(open, |icon| {
            icon.with_transformation(Transformation::rotate(radians(FRAC_PI_2)))
        })
}

/// `SidebarGroupLabel`: h-7 px-2.5 text-xs font-medium
/// text-sidebar-foreground/60. Children appended after the label (an action
/// button) sit at the right edge.
pub fn sidebar_group_label(label: impl Into<SharedString>, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .h_7()
        .pl(px(10.))
        // 6px + the action's 4px inner padding puts its icon 10px from the
        // edge, in line with the row chevrons below.
        .pr(px(6.))
        .text_size(px(12.))
        .line_height(px(16.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.sidebar_foreground.opacity(0.6))
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}

/// Small icon button for a sidebar group header (e.g. "+" next to
/// "Projects"): size-6, sidebar-foreground/60 icon, /10 hover fill.
pub fn sidebar_group_action(
    id: impl Into<ElementId>,
    name: IconName,
    theme: &Theme,
) -> Stateful<Div> {
    let fg = theme.sidebar_foreground;

    div()
        .id(id)
        .group("sidebar-group-action")
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size_6()
        .rounded(radius::SM)
        .hover(move |style| style.bg(fg.opacity(0.1)))
        .active(move |style| style.bg(fg.opacity(0.15)))
        .child(
            icon(name)
                .text_color(fg.opacity(0.6))
                .group_hover("sidebar-group-action", move |style| style.text_color(fg)),
        )
}

/// Row action for a sidebar menu button (the ⋯ on a project): size-6, only
/// visible while the row is hovered or `shown` (its menu is open).
pub fn sidebar_menu_action(
    id: impl Into<ElementId>,
    name: IconName,
    shown: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let fg = theme.sidebar_foreground;

    div()
        .id(id)
        .group("sidebar-menu-action")
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size_6()
        .rounded(radius::SM)
        .when(!shown, |action| {
            action
                .invisible()
                .group_hover("sidebar-menu-button", |style| style.visible())
        })
        .hover(move |style| style.bg(fg.opacity(0.1)))
        .active(move |style| style.bg(fg.opacity(0.15)))
        .child(
            icon(name)
                .text_color(fg.opacity(0.6))
                .group_hover("sidebar-menu-action", move |style| style.text_color(fg)),
        )
}

/// Modal backdrop: covers the window and blocks everything behind it
/// (`occlude`), but stays transparent like the site's
/// `backdrop:bg-transparent`.
pub fn modal_backdrop() -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .occlude()
}

/// `Dialog` / `AlertDialog` surface: border bg-card rounded-3xl max-w-md.
pub fn dialog_surface(id: impl Into<ElementId>, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .w(px(448.))
        .overflow_hidden()
        .rounded(radius::XL3)
        .border_1()
        .border_color(theme.border)
        .bg(theme.card)
        .text_color(theme.card_foreground)
}

/// Dialog header: p-6, title text-base font-semibold, description text-sm
/// text-muted-foreground. `gap` is gap-1.5 for Dialog, gap-2 for AlertDialog.
pub fn dialog_header(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    gap: gpui::Pixels,
    theme: &Theme,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(gap)
        .p_6()
        .child(
            div()
                .text_size(px(16.))
                .line_height(px(24.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(14.))
                .line_height(px(20.))
                .text_color(theme.muted_foreground)
                .child(description.into()),
        )
}

/// Dialog footer: border-t p-6, buttons right-aligned with gap-2.
pub fn dialog_footer(theme: &Theme) -> Div {
    div()
        .flex()
        .justify_end()
        .gap_2()
        .p_6()
        .border_t_1()
        .border_color(theme.border)
}

/// A path as people say it: `~/dev/site` rather than `/Users/me/dev/site`.
pub fn display_path(path: &std::path::Path) -> String {
    let relative = std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(|p| p.to_path_buf()));
    match relative {
        Some(relative) if relative.as_os_str().is_empty() => "~".to_string(),
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}
