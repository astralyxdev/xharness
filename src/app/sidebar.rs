//! The sidebar: the app's mark, the pages, and the projects with the
//! windows open on each one's canvas.

use super::*;

impl AppShell {

    /// The rail: transparent, sitting on the frame background.
    pub(super) fn render_sidebar(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let projects = self.projects.read(cx).projects();
        let pinned: Vec<&Project> = projects.iter().filter(|p| p.pinned).collect();
        let unpinned: Vec<&Project> = projects.iter().filter(|p| !p.pinned).collect();

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(SIDEBAR_WIDTH))
            .gap_2()
            .h_full()
            .overflow_hidden()
            .child(
                // The app's mark and name. The mark's centre lines up with
                // the menu icons below, and it has as much room above (to
                // the title bar) and below (to Home) as to its left (to the
                // window's edge): 24px each, with the gutter and gap.
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .p(px(LOGO_INSET))
                    .child(
                        // Named as an embedded asset outright: given a plain
                        // string, `img` treats anything that parses as a URI
                        // (and "logo.svg" does, as a host name) as something
                        // to download.
                        gpui::img(gpui::ImageSource::Resource(gpui::Resource::Embedded(
                            assets::LOGO_PATH.into(),
                        )))
                        .size(px(20.))
                        .rounded(px(5.))
                        .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .line_height(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.sidebar_foreground)
                            .child("XHarness"),
                    ),
            )
            .child(
                // SidebarContent: groups are gap-4 apart. Scrolls once there
                // are more projects than fit.
                div()
                    .id("sidebar-content")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap(px(16.))
                    .px(px(SIDEBAR_PADDING_X))
                    .child(
                        div().flex().flex_col().gap_1().children(Page::ALL.into_iter().map(
                            |page| {
                                ui::sidebar_menu_button(
                                    ElementId::named_usize("nav", page as usize),
                                    page.icon(),
                                    crate::i18n::t(page.label()),
                                    self.selection == Selection::Page(page),
                                    false,
                                    None,
                                    theme,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| this.show_page(page, cx)))
                            },
                        )),
                    )
                    .when(!pinned.is_empty(), |content| {
                        content.child(self.render_project_group(crate::i18n::t("Pinned"), &pinned, false, theme, cx))
                    })
                    .child(self.render_project_group(crate::i18n::t("Projects"), &unpinned, true, theme, cx)),
            )
            .into_any_element()
    }

    /// A group of project rows under a label. `can_create` adds the "+"
    /// action, and a "New project" row while there are no projects at all.
    pub(super) fn render_project_group(
        &self,
        label: &'static str,
        projects: &[&Project],
        can_create: bool,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let no_projects = self.projects.read(cx).projects().is_empty();
        let new_project = || {
            cx.listener(|this: &mut Self, _: &ClickEvent, window, cx| {
                this.open_create_dialog(window, cx)
            })
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(ui::sidebar_group_label(label, theme).when(can_create, |label| {
                    label.child(
                        ui::sidebar_group_action("new-project", IconName::Plus, theme)
                            .on_click(new_project()),
                    )
                }))
            .children(
                projects
                    .iter()
                    .map(|project| self.render_project(project, theme, cx)),
            )
            // A row as well as the "+", so an empty list says what to do.
            .when(can_create && no_projects, |group| {
                group.child(
                    ui::sidebar_menu_button(
                        "new-project-row",
                        IconName::Plus,
                        "New project",
                        false,
                        false,
                        None,
                        theme,
                    )
                    .on_click(new_project()),
                )
            })
    }

    pub(super) fn render_project(&self, project: &Project, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let id = project.id;
        let open = project.expanded;
        let selected = self.selection == Selection::Project(id);
        let menu_open = self
            .menu
            .as_ref()
            .is_some_and(|m| m.target == MenuTarget::Project(id));
        let rename = self
            .rename
            .as_ref()
            .filter(|rename| rename.project_id == id);
        let fg = theme.sidebar_foreground;
        // The sidebar is dark in both modes, so icons use the dark tints;
        // gray matches the other sidebar icons.
        let icon_color = project
            .color
            .resolve(&theme.sidebar_tints, fg.opacity(0.7));
        let row_id = ElementId::named_usize("project", id as usize);
        let offline = matches!(
            self.servers.read(cx).status(project.remote.as_ref()),
            servers::Status::Offline(_) | servers::Status::Stopping
        );

        let row = if let Some(rename) = rename {
            // Inline rename: the label becomes a field. The row stops
            // reacting to clicks so clicking in the field doesn't toggle it.
            ui::sidebar_menu_button(row_id, project.icon, "", false, true, Some(icon_color), theme)
                .bg(fg.opacity(0.1))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .h_7()
                        // Pulled left by its padding so the text stays where
                        // the label was.
                        .ml(px(-6.))
                        .px(px(6.))
                        .rounded(radius::SM)
                        .border_1()
                        .border_color(fg.opacity(0.25))
                        .text_color(fg)
                        .key_context(RENAME_CONTEXT)
                        .on_action(cx.listener(|this, _: &Confirm, _, cx| {
                            this.finish_rename(true, cx)
                        }))
                        .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                            this.finish_rename(false, cx)
                        }))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.finish_rename(true, cx)
                        }))
                        .child(rename.input.clone()),
                )
        } else {
            ui::sidebar_menu_button(
                row_id,
                project.icon,
                project.name.clone(),
                selected,
                false,
                Some(icon_color),
                theme,
            )
            // Stay highlighted while this project's menu is open.
            .when(menu_open && !selected, |row| row.bg(fg.opacity(0.1)))
            // Its server doesn't answer: the project can't open.
            .when(offline, |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .px(px(6.))
                        .rounded(px(4.))
                        .bg(theme.sidebar_tints.destructive.base.opacity(0.2))
                        .text_size(px(10.))
                        .line_height(px(16.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.sidebar_tints.destructive.soft_foreground)
                        .child("Not connected"),
                )
            })
            .map(|row| {
                row.child(
                    ui::sidebar_menu_action(
                        ElementId::named_usize("project-actions", id as usize),
                        IconName::Ellipsis,
                        menu_open,
                        theme,
                    )
                    .child(self.anchors.track(format!("project-actions-{id}")))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        let at = this.anchors.below(&format!("project-actions-{id}"), ui::Align::Start, event.position());
                        this.open_project_menu(id, at, window, cx);
                    })),
                )
                .child(
                    // The chevron only opens and closes the list.
                    div()
                        .id(ElementId::named_usize("project-chevron", id as usize))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .size_6()
                        .rounded(radius::SM)
                        .hover(move |style| style.bg(fg.opacity(0.1)))
                        .child(ui::chevron(open, theme))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_expanded(id, cx);
                        })),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| this.open_project(id, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_project_menu(id, event.position, window, cx);
                }),
            )
        };

        let active_item = self
            .canvases
            .get(&id)
            .and_then(|(canvas, _)| canvas.read(cx).active_item())
            .filter(|_| selected);

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(row)
            .when(open, |el| {
                // The windows on the project's canvas, one row each, then
                // "New window" to open another.
                let new_window_menu = self
                    .menu
                    .as_ref()
                    .is_some_and(|m| m.target == MenuTarget::NewWindow(id));
                el.children(project.items.iter().filter(|item| project.is_open(item.id)).map(|item| {
                    self.render_item_row(project, item, active_item == Some(item.id), theme, cx)
                        .into_any_element()
                }))
                .child(
                    sidebar_sub_row(
                        ElementId::named_usize("project-new-window", id as usize),
                        IconName::Plus,
                        "New window",
                        fg.opacity(if new_window_menu { 1. } else { 0.45 }),
                        None,
                        theme,
                    )
                    .when(new_window_menu, |row| row.bg(fg.opacity(0.1)))
                    .child(self.anchors.track(format!("new-window-{id}")))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        let at = this.anchors.below(&format!("new-window-{id}"), ui::Align::Start, event.position());
                        this.open_new_window_menu(id, at, window, cx)
                    })),
                )
            })
            .into_any_element()
    }

    /// A window under its project: kind icon, title, and its open status (a
    /// green dot while it's open; closed windows are dimmed).
    pub(super) fn render_item_row(
        &self,
        project: &Project,
        item: &ProjectItem,
        active: bool,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let (project_id, item_id) = (project.id, item.id);
        let fg = theme.sidebar_foreground;
        let open = project.is_open(item_id);
        let color = match (active, open) {
            (true, _) => fg,
            (false, true) => fg.opacity(0.7),
            (false, false) => fg.opacity(0.4),
        };
        let menu_open = self
            .menu
            .as_ref()
            .is_some_and(|m| m.target == MenuTarget::Item(item_id));

        // The window's own color tints its icon (gray follows the row).
        let icon_color = item.color.resolve(&theme.sidebar_tints, color);
        let icon_color = if open || active { icon_color } else { icon_color.opacity(0.6) };

        sidebar_sub_row(
            ElementId::named_usize("item", item_id as usize),
            item.kind.icon(),
            item.title.clone(),
            color,
            Some(icon_color),
            theme,
        )
        .when(active, |row| row.bg(fg.opacity(0.15)))
        .when(menu_open && !active, |row| row.bg(fg.opacity(0.1)))
        .when(open, |row| {
            row.child(ui::status_dot(theme.sidebar_tints.green.base))
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_item(project_id, item_id, window, cx)
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                this.open_item_menu(project_id, item_id, event.position, window, cx);
            }),
        )
    }
}

/// A row nested under a project (Tree rows: h-7 text-sm). Its icon sits where
/// the project's name starts.
fn sidebar_sub_row(
    id: ElementId,
    icon: IconName,
    label: impl Into<gpui::SharedString>,
    color: gpui::Hsla,
    icon_color: Option<gpui::Hsla>,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let fg = theme.sidebar_foreground;
    div()
        .id(id)
        .group("sidebar-sub-row")
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(6.))
        .h_7()
        .ml(px(MENU_LABEL_INSET - 10.))
        .px(px(10.))
        .rounded(radius::MD)
        .text_size(px(13.))
        .line_height(px(18.))
        .text_color(color)
        .hover(move |style| style.bg(fg.opacity(0.1)).text_color(fg))
        .child(match icon_color {
            Some(icon_color) => ui::icon(icon).size_3p5().text_color(icon_color),
            None => ui::icon(icon)
                .size_3p5()
                .text_color(color)
                .group_hover("sidebar-sub-row", move |style| style.text_color(fg)),
        })
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}
