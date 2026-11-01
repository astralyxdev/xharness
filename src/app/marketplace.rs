//! The Marketplace page, as in Zed's extensions: mods (the kinds of
//! windows; see `windows/mod.rs`) and language servers, which give the editor
//! errors as you type, completions and go to definition (see `services/lsp.rs`).

use gpui::{App, Context, FontWeight, Task, Window, div, prelude::*, px, relative};

use crate::windows::{MODS, Mod, Mods};

use crate::ui::assets::IconName;
use crate::services::language_servers::{self, CATALOG, Installed, Source, Spec};
use crate::services::lsp::Lsp;
use crate::ui::theme::{Theme, radius};
use crate::ui::{self, Button, Variant};

#[derive(Clone)]
enum State {
    Checking,
    Ready(Installed),
    Installing,
    Removing,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Mods,
    LanguageServers,
}

pub struct MarketplaceView {
    tab: Tab,
    states: Vec<State>,
    _check: Option<Task<()>>,
}

impl MarketplaceView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            tab: Tab::Mods,
            states: vec![State::Checking; CATALOG.len()],
            _check: None,
        };
        this.refresh(cx);
        this
    }

    /// Checks what's installed again (in the background: it can look on
    /// your PATH).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self._check = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { CATALOG.iter().map(language_servers::installed).collect::<Vec<_>>() })
                .await;
            this.update(cx, |this, cx| {
                for (state, found) in this.states.iter_mut().zip(found) {
                    if matches!(state, State::Checking | State::Ready(_)) {
                        *state = State::Ready(found);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn run(&mut self, ix: usize, install: bool, cx: &mut Context<Self>) {
        let spec: &'static Spec = &CATALOG[ix];
        self.states[ix] = if install { State::Installing } else { State::Removing };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = if install { language_servers::install(spec) } else { language_servers::uninstall(spec) };
                    result.map(|()| language_servers::installed(spec))
                })
                .await;
            this.update(cx, |this, cx| {
                this.states[ix] = match result {
                    Ok(found) => State::Ready(found),
                    Err(error) => State::Failed(error),
                };
                // Open editors pick up (or drop) the server.
                Lsp::global(cx).update(cx, |lsp, cx| lsp.servers_changed(cx));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_mod(&self, ix: usize, item: &Mod, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let installed = Mods::installed(item.kind, cx);
        let kind = item.kind;
        let button = if installed {
            Button::new(("mod-uninstall", ix), "Uninstall")
                .variant(Variant::Soft)
                .on_click(cx.listener(move |_, _, _, cx| {
                    Mods::set(kind, false, cx);
                    cx.notify();
                }))
        } else {
            Button::new(("mod-install", ix), "Install")
                .icon(IconName::Download)
                .on_click(cx.listener(move |_, _, _, cx| {
                    Mods::set(kind, true, cx);
                    cx.notify();
                }))
        };
        let (status, tint) = if installed {
            ("Installed · built in", Some(theme.tints.green))
        } else {
            ("Not installed", None)
        };
        card(theme)
            .child(card_title(item.icon(), item.name, "Window · built in".to_string(), theme.tints.blue, theme))
            .child(card_text(item.description, theme))
            .child(card_footer(status.to_string(), tint, Some(button), theme))
    }

    fn render_card(&self, ix: usize, spec: &Spec, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let state = &self.states[ix];
        let how = match spec.source {
            Source::GithubGz { .. } | Source::GithubBinary { .. } => "GitHub release",
            Source::Npm { .. } => "npm",
            Source::Go { .. } => "go install",
            Source::System => "Xcode tools",
        };
        let (status, tint): (String, Option<_>) = match state {
            State::Checking => ("Checking…".to_string(), None),
            State::Ready(Installed::Here) => ("Installed".to_string(), Some(theme.tints.green)),
            State::Ready(Installed::System(path)) => (format!("On your system · {}", ui::display_path(path)), Some(theme.tints.blue)),
            State::Ready(Installed::No) => ("Not installed".to_string(), None),
            State::Installing => ("Installing…".to_string(), Some(theme.tints.amber)),
            State::Removing => ("Removing…".to_string(), Some(theme.tints.amber)),
            State::Failed(_) => ("Didn't install".to_string(), Some(theme.tints.destructive)),
        };
        let button = match state {
            State::Ready(Installed::Here) => Some(
                Button::new(("uninstall", ix), "Uninstall")
                    .variant(Variant::Soft)
                    .on_click(cx.listener(move |this, _, _, cx| this.run(ix, false, cx))),
            ),
            State::Ready(Installed::No) | State::Failed(_) if !matches!(spec.source, Source::System) => Some(
                Button::new(("install", ix), "Install")
                    .icon(IconName::Download)
                    .on_click(cx.listener(move |this, _, _, cx| this.run(ix, true, cx))),
            ),
            _ => None,
        };
        card(theme)
            .child(card_title(IconName::FileCode, spec.name, format!("{} · {how}", spec.languages), theme.tints.violet, theme))
            .child(card_text(spec.description, theme))
            .children(match state {
                State::Failed(error) => Some(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.tints.destructive.soft_foreground)
                        .child(error.clone()),
                ),
                _ => None,
            })
            .child(card_footer(status, tint, button, theme))
    }
}

fn card(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .w(px(340.))
        .p_4()
        .rounded(radius::XL)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
}

fn card_title(icon: IconName, name: &'static str, subtitle: String, tint: crate::ui::theme::Tint, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .size(px(36.))
                .rounded(radius::LG)
                .bg(tint.base.opacity(0.14))
                .child(ui::icon(icon).text_color(tint.soft_foreground)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(name))
                .child(div().text_size(px(12.)).text_color(theme.muted_foreground).child(subtitle)),
        )
}

fn card_text(text: &'static str, theme: &Theme) -> gpui::Div {
    div()
        .text_size(px(13.))
        .line_height(relative(1.45))
        .text_color(theme.muted_foreground)
        .child(text)
}

fn card_footer(status: String, tint: Option<crate::ui::theme::Tint>, button: Option<Button>, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .mt_auto()
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_1p5()
                .text_size(px(12.))
                .text_color(tint.map_or(theme.muted_foreground, |t| t.soft_foreground))
                .when_some(tint, |el, t| el.child(ui::status_dot(t.base)))
                .child(div().truncate().child(status)),
        )
        .children(button)
}

impl Render for MarketplaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        div()
            .id("marketplace")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .px(px(40.))
                    .py(px(36.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_size(px(28.)).font_weight(FontWeight::SEMIBOLD).child("Marketplace"))
                            .child(
                                div()
                                    .max_w(px(620.))
                                    .text_size(px(14.))
                                    .line_height(relative(1.5))
                                    .text_color(theme.muted_foreground)
                                    .child(match self.tab {
                                        Tab::Mods => "Mods are the kinds of windows a canvas can have. They come \
                                                      installed; uninstall the ones you don't use, and they leave \
                                                      \u{201c}New window\u{201d}.",
                                        Tab::LanguageServers => "Install a language server and editors for its \
                                                      languages show errors as you type, complete as you go \
                                                      (ctrl-space), and jump to definitions (F12 or ⌘-click). \
                                                      Syntax highlighting works without one.",
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .p_1()
                            .rounded(radius::LG)
                            .bg(theme.secondary)
                            .w(px(300.))
                            .children([(Tab::Mods, "Mods"), (Tab::LanguageServers, "Language servers")].map(|(tab, label)| {
                                let active = self.tab == tab;
                                div()
                                    .id(label)
                                    .flex_1()
                                    .flex()
                                    .justify_center()
                                    .py_1()
                                    .rounded(radius::MD)
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .cursor_pointer()
                                    .when(active, |el| el.bg(theme.background).text_color(theme.foreground))
                                    .when(!active, |el| el.text_color(theme.muted_foreground))
                                    .child(label)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.tab = tab;
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(div().flex().flex_wrap().gap_4().map(|grid| match self.tab {
                        Tab::Mods => grid.children(MODS.iter().enumerate().map(|(ix, item)| self.render_mod(ix, item, theme, cx).into_any_element())),
                        Tab::LanguageServers => grid.children(
                            CATALOG.iter().enumerate().map(|(ix, spec)| self.render_card(ix, spec, theme, cx).into_any_element()),
                        ),
                    })),
            )
    }
}

/// So the page can be created before its first look.
pub fn new(cx: &mut App) -> gpui::Entity<MarketplaceView> {
    cx.new(MarketplaceView::new)
}
