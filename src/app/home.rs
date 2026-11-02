//! The Home page: a greeting, quick actions, your projects as cards, and a
//! few things worth knowing.

use super::settings::Settings;
use super::*;
use crate::i18n::{t, tf, tn};

impl AppShell {
    pub(super) fn render_home(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let settings = Settings::get(cx);
        let name = settings.display_name();
        let first_name = name.split_whitespace().next().unwrap_or(&name).to_string();
        let projects = self.projects.read(cx).projects();

        div()
            .id("home")
            .flex_1()
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(36.))
                    .max_w(px(980.))
                    .mx_auto()
                    .px(px(48.))
                    .py(px(48.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child(t("XHARNESS · YOUR WORKSPACE")),
                            )
                            .child(
                                div()
                                    .text_size(px(40.))
                                    .line_height(relative(1.05))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(tf("Welcome back, {}", &[&first_name])),
                            )
                            .child(
                                div()
                                    .max_w(px(560.))
                                    .text_size(px(15.))
                                    .line_height(relative(1.6))
                                    .text_color(theme.muted_foreground)
                                    .child(t(
                                        "Each project is a canvas: terminals, files, git and browsers side by side, with notes and sections to keep it organised. Shells keep running when the app quits.",
                                    )),
                            )
                            .child(
                                div()
                                    .mt_3()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("home-new-project", t("New project"))
                                            .icon(IconName::Plus)
                                            .size(ButtonSize::Large)
                                            .on_click(cx.listener(|this, _, window, cx| this.open_create_dialog(window, cx))),
                                    )
                                    .child(
                                        Button::new("home-palette", t("Command palette"))
                                            .icon(IconName::Command)
                                            .variant(Variant::Soft)
                                            .size(ButtonSize::Large)
                                            .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx))),
                                    )
                                    .child(
                                        Button::new("home-settings", t("Settings"))
                                            .icon(IconName::Settings)
                                            .variant(Variant::Ghost)
                                            .size(ButtonSize::Large)
                                            .on_click(cx.listener(|this, _, _, cx| this.show_page(Page::Settings, cx))),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(heading(t("Projects"), theme).when(!projects.is_empty(), |el| {
                                el.child(div().text_color(theme.muted_foreground).font_weight(FontWeight::NORMAL).child(projects.len().to_string()))
                            }))
                            .child(if projects.is_empty() {
                                div()
                                    .p_6()
                                    .rounded(radius::XL)
                                    .border_1()
                                    .border_color(theme.border)
                                    .text_size(px(14.))
                                    .text_color(theme.muted_foreground)
                                    .child(t("No projects yet. Make one from a folder to get its canvas."))
                                    .into_any_element()
                            } else {
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_3()
                                    .children(projects.iter().map(|project| self.render_project_card(project, theme, cx)))
                                    .into_any_element()
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(heading(t("Good to know"), theme))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_3()
                                    .child(tip(IconName::Command, t("Everything, by name"), t("⌘K finds any command, window or project."), theme))
                                    .child(tip(IconName::StickyNote, t("Mark the canvas up"), t("Text, notes with checkboxes, and sections that move what's inside them."), theme))
                                    .child(tip(IconName::Link, t("Drop files on a terminal"), t("It goes to that folder, stopping what's running. Hold ⌥ to type the path instead."), theme))
                                    .child(tip(IconName::Languages, t("English or Russian"), t("Switch in Settings or from your profile menu."), theme)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_project_card(&self, project: &Project, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let id = project.id;
        let color = project.color.resolve(&theme.tints, theme.muted_foreground);
        let open = project.windows.len();
        let place = match (&project.remote, &project.folder) {
            (Some(key), Some(folder)) => format!("{} · {}", key.name, folder.display()),
            (Some(key), None) => key.name.clone(),
            (None, Some(folder)) => ui::display_path(folder),
            (None, None) => t("No folder").to_string(),
        };
        let hover = theme.border_active;
        div()
            .id(ElementId::named_usize("home-project", id as usize))
            .flex()
            .flex_col()
            .gap_3()
            .w(px(280.))
            .p_4()
            .rounded(radius::XL)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .cursor_pointer()
            .hover(move |style| style.border_color(hover))
            .on_click(cx.listener(move |this, _, window, cx| this.open_project(id, window, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(36.))
                            .rounded(radius::MD)
                            .bg(color.opacity(0.14))
                            .child(ui::icon(project.icon).size_5().text_color(color)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).truncate().child(project.name.clone()))
                            .child(div().text_size(px(12.)).text_color(theme.muted_foreground).truncate().child(place)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(ui::status_dot(if open > 0 { theme.tints.green.base } else { theme.muted_foreground.opacity(0.5) }))
                    .child(tn(open, "{} window open", "{} windows open"))
                    .when(!project.marks.is_empty(), |el| el.child("·").child(tn(project.marks.len(), "{} mark", "{} marks")))
                    .when(project.pinned, |el| el.child(ui::icon(IconName::Pin).size_3().text_color(theme.muted_foreground))),
            )
    }
}

fn heading(text: &str, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(15.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(text.to_string())
}

fn tip(icon: IconName, title: &str, text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .gap_3()
        .w(px(430.))
        .p_4()
        .rounded(radius::XL)
        .bg(theme.muted)
        .child(ui::icon(icon).mt(px(2.)).text_color(theme.foreground))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
                .child(div().text_size(px(13.)).line_height(relative(1.5)).text_color(theme.muted_foreground).child(text.to_string())),
        )
}
