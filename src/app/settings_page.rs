//! The Settings page: your profile, the app's language and look, the
//! canvas, the keyboard shortcuts, and where things are kept.

use gpui::{AnyElement, App, Context, Entity, FontWeight, SharedString, Subscription, Task, Window, div, prelude::*, px, relative};

use super::settings::{Appearance, Settings};
use crate::i18n::{Language, t, tf};
use crate::services::github::{self, SignedIn};
use crate::services::projects::ProjectColor;
use crate::ui::assets::IconName;
use crate::ui::text_input::{TextInput, TextInputEvent};
use crate::ui::theme::{Theme, radius};
use crate::ui::{self, Button, Segment, Variant};

pub struct SettingsView {
    name: Entity<TextInput>,
    email: Entity<TextInput>,
    /// Who git is signed in to GitHub as (`None` while asking).
    github: Option<Result<SignedIn, String>>,
    _check: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// The keyboard shortcuts, as Settings lists them.
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("Cmd+K", "Command palette"),
    ("Cmd+T", "New terminal"),
    ("Cmd+Shift+D", "New terminal below"),
    ("Cmd+W", "Close window"),
    ("Cmd+=", "Zoom in"),
    ("Cmd+-", "Zoom out"),
    ("Cmd+0", "Zoom to 100%"),
    ("Cmd+1", "Zoom to fit everything"),
    ("Cmd+2", "Zoom to the active window"),
    ("V", "Select (on empty canvas)"),
    ("H", "Hand: drag to pan"),
    ("T", "Add text"),
    ("N", "Add a note"),
    ("S", "Add a section"),
    ("Delete", "Delete the selected mark"),
    ("Cmd+L", "Browser: address bar"),
    ("Cmd+R", "Browser: reload"),
];

impl SettingsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let settings = Settings::get(cx).clone();
        let (git_name, git_email) = settings.git_identity();
        let name_placeholder = if git_name.is_empty() { t("Your name").to_string() } else { git_name.to_string() };
        let email_placeholder = if git_email.is_empty() { "you@example.com".to_string() } else { git_email.to_string() };
        let name = cx.new(|cx| TextInput::new(name_placeholder, cx).with_text(settings.name.clone()));
        let email = cx.new(|cx| TextInput::new(email_placeholder, cx).with_text(settings.email.clone()));
        let subscriptions = vec![
            cx.subscribe(&name, |_, input, _: &TextInputEvent, cx| {
                let text = input.read(cx).text().to_string();
                Settings::update(cx, |settings| settings.name = text);
            }),
            cx.subscribe(&email, |_, input, _: &TextInputEvent, cx| {
                let text = input.read(cx).text().to_string();
                Settings::update(cx, |settings| settings.email = text);
            }),
            cx.observe_global::<Settings>(|_, cx| cx.notify()),
        ];
        let mut this = Self {
            name,
            email,
            github: None,
            _check: None,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    /// Asks again who git is signed in to GitHub as (on this Mac's server).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self._check = Some(cx.spawn(async move |this, cx| {
            let accounts = cx.background_executor().spawn(async { github::accounts().map(|a| a.signed_in) }).await;
            this.update(cx, |this, cx| {
                this.github = Some(accounts);
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_profile(&self, settings: &Settings, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let color = settings.avatar_color.resolve(&theme.tints, theme.muted_foreground);
        let github: SharedString = match &self.github {
            None => t("Checking…").into(),
            Some(Ok(SignedIn::Yes(account))) => tf("Signed in as @{}", &[&account.login]).into(),
            Some(Ok(SignedIn::Unchecked { login, .. })) => tf("Signed in as @{} (not checked)", &[login]).into(),
            Some(Ok(SignedIn::Rejected(login))) => tf("GitHub turned down @{}'s token: sign in again from a Git window", &[login]).into(),
            Some(Ok(SignedIn::No)) => t("Not signed in. Sign in from a project's Git window.").into(),
            Some(Err(error)) => error.clone().into(),
        };
        section(
            t("Profile"),
            t("How you appear in XHarness. Empty fields use your git name and email."),
            theme,
        )
        .child(
            card(theme)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_4()
                        .p_4()
                        .child(ui::avatar(settings.initials(), color, 48.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .min_w_0()
                                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).truncate().child(settings.display_name()))
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(settings.display_email()),
                                ),
                        ),
                )
                .child(row(t("Name"), None, theme).child(div().w(px(260.)).child(self.name.clone())))
                .child(row(t("Email"), None, theme).child(div().w(px(260.)).child(self.email.clone())))
                .child(
                    row(t("Avatar color"), None, theme).child(
                        div().flex().gap_1p5().children(ProjectColor::ALL.into_iter().filter(|c| *c != ProjectColor::Gray).map(|choice| {
                            let selected = settings.avatar_color == choice;
                            let fill = choice.resolve(&theme.tints, theme.muted_foreground);
                            div()
                                .id(SharedString::from(format!("avatar-color-{choice:?}")))
                                .size(px(22.))
                                .rounded_full()
                                .bg(fill)
                                .cursor_pointer()
                                .border_2()
                                .border_color(if selected { theme.foreground } else { gpui::transparent_black() })
                                .on_click(move |_, _, cx| Settings::update(cx, |s| s.avatar_color = choice))
                        })),
                    ),
                )
                .child(row(t("GitHub"), Some(github), theme).child(
                    Button::new("settings-github-refresh", t("Check again"))
                        .variant(Variant::Soft)
                        .icon(IconName::RefreshCw)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.github = None;
                            this.refresh(cx);
                            cx.notify();
                        })),
                )),
        )
        .into_any_element()
    }

    fn render_interface(&self, settings: &Settings, theme: &Theme) -> AnyElement {
        let language = settings.language();
        let languages = Language::ALL
            .into_iter()
            .map(|choice| {
                Segment::new(choice.native_name(), language == choice, move |_, _, cx| {
                    Settings::update(cx, |s| s.language = Some(choice))
                })
            })
            .collect();
        let appearances = Appearance::ALL
            .into_iter()
            .map(|choice| {
                let icon = match choice {
                    Appearance::System => IconName::SunMoon,
                    Appearance::Light => IconName::Sun,
                    Appearance::Dark => IconName::Moon,
                };
                Segment::new(t(choice.label()), settings.appearance == choice, move |_, _, cx| {
                    Settings::update(cx, |s| s.appearance = choice)
                })
                .icon(icon)
            })
            .collect();
        section(t("Interface"), t("The app's language and colors."), theme)
            .child(
                card(theme)
                    .child(
                        row(t("Language"), Some(t("Menus, windows and messages.").into()), theme)
                            .child(ui::segmented("settings-language", languages, theme)),
                    )
                    .child(row(t("Appearance"), None, theme).child(ui::segmented("settings-appearance", appearances, theme))),
            )
            .into_any_element()
    }

    fn render_canvas(&self, settings: &Settings, theme: &Theme) -> AnyElement {
        let (dots, snap) = (settings.dot_grid, settings.snap_to_grid);
        section(t("Canvas"), t("How project canvases look and how things move on them."), theme)
            .child(
                card(theme)
                    .child(
                        row(t("Dot grid"), Some(t("Dots on empty canvas.").into()), theme).child(
                            ui::switch("settings-dots", dots, theme)
                                .on_click(move |_, _, cx| Settings::update(cx, |s| s.dot_grid = !dots)),
                        ),
                    )
                    .child(
                        row(
                            t("Snap to grid"),
                            Some(t("Windows, notes and sections line up on the grid as you move and resize them.").into()),
                            theme,
                        )
                        .child(
                            ui::switch("settings-snap", snap, theme)
                                .on_click(move |_, _, cx| Settings::update(cx, |s| s.snap_to_grid = !snap)),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_shortcuts(&self, theme: &Theme) -> AnyElement {
        section(t("Keyboard shortcuts"), t("Single letters work while no window is active."), theme)
            .child(card(theme).children(SHORTCUTS.iter().map(|(keys, what)| {
                row(t(what), None, theme).py(px(8.)).child(ui::kbd(ui::shortcut(keys), theme))
            })))
            .into_any_element()
    }

    fn render_about(&self, theme: &Theme) -> AnyElement {
        let data = crate::services::paths::data_dir();
        let shown = data.as_deref().map(ui::display_path).unwrap_or_default();
        section(t("About"), t("XHarness and where it keeps its files."), theme)
            .child(
                card(theme)
                    .child(row(t("Version"), None, theme).child(
                        div().text_size(px(13.)).text_color(theme.muted_foreground).child(env!("CARGO_PKG_VERSION")),
                    ))
                    .child(row(t("Data folder"), Some(shown.into()), theme).child(
                        Button::new("settings-reveal-data", t("Show"))
                            .variant(Variant::Soft)
                            .icon(IconName::FolderOpen)
                            .on_click(move |_, _, cx| {
                                if let Some(dir) = &data {
                                    cx.reveal_path(dir);
                                }
                            }),
                    )),
            )
            .into_any_element()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx).clone();
        let settings = Settings::get(cx).clone();
        div()
            .id("settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(32.))
                    .max_w(px(760.))
                    .mx_auto()
                    .px(px(40.))
                    .py(px(40.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_size(px(28.)).line_height(px(34.)).font_weight(FontWeight::SEMIBOLD).child(t("Settings")))
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .text_color(theme.muted_foreground)
                                    .child(t("Changes apply right away and are saved on this computer.")),
                            ),
                    )
                    .child(self.render_profile(&settings, &theme, cx))
                    .child(self.render_interface(&settings, &theme))
                    .child(self.render_canvas(&settings, &theme))
                    .child(self.render_shortcuts(&theme))
                    .child(self.render_about(&theme)),
            )
    }
}

/// A titled group of settings.
fn section(title: &str, description: &str, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(px(16.)).line_height(px(22.)).font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(relative(1.45))
                        .text_color(theme.muted_foreground)
                        .child(description.to_string()),
                ),
        )
}

/// A card of rows, divided by lines.
fn card(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .rounded(radius::XL)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .overflow_hidden()
}

/// A setting: its name (and what it does) on the left, its control on the
/// right (the caller adds it as a child).
fn row(label: &str, description: Option<SharedString>, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_6()
        .px_4()
        .py_3()
        .border_t_1()
        .border_color(theme.border)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .min_w_0()
                .flex_1()
                .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child(label.to_string()))
                .children(description.map(|d| {
                    div().text_size(px(12.5)).line_height(relative(1.4)).text_color(theme.muted_foreground).child(d)
                })),
        )
}

pub fn new(cx: &mut App) -> Entity<SettingsView> {
    cx.new(SettingsView::new)
}
