//! The title bar: the command search in the middle (it opens the command
//! palette) and your profile at the right, with its menu. And the
//! palette's list of commands.

use super::palette::{Command, CommandPalette, PaletteEvent};
use super::settings::{Appearance, Settings};
use super::*;
use crate::canvas::{CanvasCommand, Tool};
use crate::i18n::{Language, t, tf};
use crate::services::projects::MarkKind;

impl AppShell {
    pub(super) fn render_titlebar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let settings = Settings::get(cx);
        let avatar_color = settings.avatar_color.resolve(&theme.sidebar_tints, theme.sidebar_foreground);
        let name = settings.display_name();
        let initials = settings.initials();
        let place = match self.selection {
            Selection::Project(id) => self.project(id, cx).map(|p| p.name.clone()),
            Selection::Page(page) => Some(t(page.label()).to_string()),
        };
        let white = gpui::white();
        // The title bar, with the traffic lights at its left. macOS drags
        // the window from it; double-click zooms. Elsewhere it's a drag
        // area with our own caption buttons, which the system handles (so
        // Windows' snap layouts work on Maximize).
        div()
            .id("titlebar")
            .relative()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .h(px(TITLEBAR_HEIGHT))
            .bg(gpui::rgb(TITLEBAR_COLOR))
            .text_size(px(12.))
            .line_height(px(16.))
            .text_color(white)
            .map(|bar| {
                if cfg!(target_os = "macos") {
                    bar.on_click(titlebar_double_click)
                } else {
                    bar.window_control_area(WindowControlArea::Drag)
                }
            })
            .child(
                // The command search: where you are, and ⌘K.
                div()
                    .id("command-center")
                    .flex()
                    .items_center()
                    .gap_2()
                    .w(px(360.))
                    .h(px(22.))
                    .px(px(10.))
                    .rounded(px(7.))
                    .bg(white.opacity(0.12))
                    .border_1()
                    .border_color(white.opacity(0.14))
                    .cursor_pointer()
                    .hover(|style| style.bg(white.opacity(0.18)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_palette(window, cx);
                    }))
                    .child(ui::icon(IconName::Search).size_3p5().text_color(white.opacity(0.75)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(white.opacity(0.9))
                            .child(match place {
                                Some(place) => tf("XHarness — {}", &[&place]),
                                None => "XHarness".to_string(),
                            }),
                    )
                    .child(div().text_color(white.opacity(0.6)).child(ui::shortcut("Cmd+K"))),
            )
            .child(
                // Your profile, at the right (before Windows' caption
                // buttons).
                div()
                    .id("profile")
                    .absolute()
                    .top(px(4.))
                    .right(px(if cfg!(target_os = "macos") { 8. } else { 46. * 3. + 8. }))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(24.))
                    .pl(px(3.))
                    .pr(px(8.))
                    .rounded_full()
                    .cursor_pointer()
                    .hover(|style| style.bg(white.opacity(0.14)))
                    .child(self.anchors.track("profile"))
                    .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        // Under the button, its right edge on the button's.
                        let at = this.anchors.below("profile", ui::Align::End, event.position());
                        this.open_profile_menu(at, window, cx);
                    }))
                    .child(ui::avatar(initials, avatar_color, 18.).border_1().border_color(white.opacity(0.5)))
                    .child(div().max_w(px(160.)).truncate().font_weight(FontWeight::MEDIUM).child(name))
                    .child(ui::icon(IconName::ChevronDown).size_3().text_color(white.opacity(0.7))),
            )
            .when(!cfg!(target_os = "macos"), |bar| bar.child(caption_buttons()))
    }

    fn open_profile_menu(&mut self, position: crate::ui::context_menu::MenuAt, window: &mut Window, cx: &mut Context<Self>) {
        let theme = Theme::global(cx);
        let settings = Settings::get(cx).clone();
        let shell = cx.entity().downgrade();
        let avatar = settings.avatar_color.resolve(&theme.tints, theme.muted_foreground);
        let mut entries = vec![
            MenuEntry::Header {
                title: settings.display_name().into(),
                subtitle: settings.display_email().into(),
                avatar: Some((settings.initials().into(), avatar)),
            },
            MenuEntry::Separator,
            MenuEntry::item(IconName::Settings, t("Settings"), {
                let shell = shell.clone();
                move |_, cx| {
                    shell.update(cx, |this, cx| this.show_page(Page::Settings, cx)).ok();
                }
            }),
            MenuEntry::item(IconName::Command, t("Command palette"), {
                let shell = shell.clone();
                move |window, cx| {
                    shell.update(cx, |this, cx| this.open_palette(window, cx)).ok();
                }
            }),
            MenuEntry::Separator,
        ];
        let language = settings.language();
        for choice in Language::ALL {
            entries.push(MenuEntry::check(choice.native_name(), language == choice, move |_, cx| {
                Settings::update(cx, |s| s.language = Some(choice))
            }));
        }
        entries.push(MenuEntry::Separator);
        for choice in Appearance::ALL {
            entries.push(MenuEntry::check(t(choice.label()), settings.appearance == choice, move |_, cx| {
                Settings::update(cx, |s| s.appearance = choice)
            }));
        }
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::item(IconName::LogOut, t("Quit XHarness"), move |window, cx| {
            shell.update(cx, |this, cx| this.quit(window, cx)).ok();
        }));
        self.show_menu(MenuTarget::Profile, entries, position, window, cx);
    }

    pub(super) fn show_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.selection = Selection::Page(page);
        match page {
            Page::Marketplace => self.marketplace.update(cx, |m, cx| m.refresh(cx)),
            Page::Settings => self.settings_view.update(cx, |s, cx| s.refresh(cx)),
            Page::Home => {}
        }
        cx.notify();
    }

    /// Opens the command palette (⌘K).
    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let commands = self.commands(window, cx);
        let palette = cx.new(|cx| CommandPalette::new(commands, window, cx));
        let subscription = cx.subscribe_in(&palette, window, |this, palette, event: &PaletteEvent, window, cx| {
            this.close_modal(palette.entity_id(), cx);
            if let PaletteEvent::Run(run) = event {
                let run = run.clone();
                // After the palette is gone, so the command can open menus
                // and dialogs, and focus what it opens.
                window.defer(cx, move |window, cx| run(window, cx));
            } else {
                this.focus_selection(window, cx);
            }
        });
        self.menu = None;
        self.modal = Some(Modal {
            view: palette.into(),
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Gives the keyboard back to what's showing.
    fn focus_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Selection::Project(id) = self.selection
            && let Some((canvas, _)) = self.canvases.get(&id)
        {
            canvas.update(cx, |canvas, cx| canvas.focus(window, cx));
        }
    }

    /// Everything the palette offers, for what's showing now.
    fn commands(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Vec<Command> {
        let shell = cx.entity().downgrade();
        let mut commands = Vec::new();
        let on_shell = |f: Box<dyn Fn(&mut Self, &mut Window, &mut Context<Self>)>| {
            let shell = shell.clone();
            move |window: &mut Window, cx: &mut App| {
                shell.update(cx, |this, cx| f(this, window, cx)).ok();
            }
        };

        // The canvas showing now.
        if let Selection::Project(project) = self.selection {
            let canvas_command = |command: CanvasCommand| {
                on_shell(Box::new(move |this: &mut Self, window, cx| {
                    let canvas = this.canvas(project, window, cx);
                    canvas.update(cx, |canvas, cx| canvas.run(command, window, cx));
                }))
            };
            let installed = |kind| Mods::installed(kind, cx);
            if installed(ItemKind::Terminal) {
                commands.push(Command::new(IconName::SquareTerminal, t("New terminal"), "Canvas", on_shell(Box::new(move |this, window, cx| this.new_terminal(project, window, cx)))).keys("Cmd+T"));
                commands.push(Command::new(IconName::Rows, t("New terminal below"), "Canvas", canvas_command(CanvasCommand::NewTerminalBelow)).keys("Cmd+Shift+D"));
            }
            if installed(ItemKind::Explorer) {
                commands.push(Command::new(IconName::FolderTree, t("New explorer"), "Canvas", on_shell(Box::new(move |this, window, cx| this.new_explorer(project, window, cx)))));
            }
            if installed(ItemKind::Browser) {
                commands.push(Command::new(IconName::Globe, t("New browser"), "Canvas", on_shell(Box::new(move |this, window, cx| this.new_browser(project, window, cx)))));
            }
            if installed(ItemKind::Sftp) {
                commands.push(Command::new(IconName::Server, t("New SFTP window"), "Canvas", on_shell(Box::new(move |this, window, cx| this.new_sftp(project, window, cx)))));
            }
            let has_folder = self.project(project, cx).is_some_and(|p| p.folder.is_some());
            if has_folder && installed(ItemKind::Git) {
                commands.push(Command::new(IconName::GitBranch, t("Open Git"), "Canvas", on_shell(Box::new(move |this, window, cx| this.open_kind(project, ItemKind::Git, window, cx)))));
            }
            if has_folder && installed(ItemKind::Lino) {
                commands.push(Command::new(IconName::Activity, t("Open Lino"), "Canvas", on_shell(Box::new(move |this, window, cx| this.open_kind(project, ItemKind::Lino, window, cx)))));
            }
            commands.push(Command::new(IconName::Type, t("Add text"), "Canvas", canvas_command(CanvasCommand::AddMark(MarkKind::Text))));
            commands.push(Command::new(IconName::StickyNote, t("Add a note"), "Canvas", canvas_command(CanvasCommand::AddMark(MarkKind::Note))));
            commands.push(Command::new(IconName::Frame, t("Add a section"), "Canvas", canvas_command(CanvasCommand::AddMark(MarkKind::Section))));
            commands.push(Command::new(IconName::Hand, t("Hand tool"), "Canvas", canvas_command(CanvasCommand::Tool(Tool::Hand))).keys("H"));
            commands.push(Command::new(IconName::LayoutGrid, t("Tidy up windows"), "Canvas", canvas_command(CanvasCommand::Tidy)));
            commands.push(Command::new(IconName::Maximize, t("Zoom to fit"), "Canvas", canvas_command(CanvasCommand::ZoomToFit)).keys("Cmd+1"));
            commands.push(Command::new(IconName::Search, t("Zoom to the active window"), "Canvas", canvas_command(CanvasCommand::ZoomToWindow)).keys("Cmd+2"));
            commands.push(Command::new(IconName::Plus, t("Zoom in"), "Canvas", canvas_command(CanvasCommand::ZoomIn)).keys("Cmd+="));
            commands.push(Command::new(IconName::Minus, t("Zoom out"), "Canvas", canvas_command(CanvasCommand::ZoomOut)).keys("Cmd+-"));
            commands.push(Command::new(IconName::Square, t("Zoom to 100%"), "Canvas", canvas_command(CanvasCommand::ZoomToActualSize)).keys("Cmd+0"));
            commands.push(Command::new(IconName::X, t("Close window"), "Canvas", canvas_command(CanvasCommand::CloseWindow)).keys("Cmd+W"));

            // Its windows.
            if let Some((canvas, _)) = self.canvases.get(&project) {
                for (item, kind, title) in canvas.read(cx).open_windows(cx) {
                    commands.push(Command::new(kind.icon(), title, "Window", on_shell(Box::new(move |this, window, cx| this.open_item(project, item, window, cx)))));
                }
            }
        }

        // Projects.
        for project in self.projects.read(cx).projects() {
            let id = project.id;
            commands.push(Command::new(project.icon, project.name.clone(), "Project", on_shell(Box::new(move |this, window, cx| this.open_project(id, window, cx)))));
        }
        commands.push(Command::new(IconName::Plus, t("New project…"), "App", on_shell(Box::new(|this, window, cx| this.open_create_dialog(window, cx)))));

        // Pages and settings.
        commands.push(Command::new(IconName::House, t("Home"), "App", on_shell(Box::new(|this, _, cx| this.show_page(Page::Home, cx)))));
        commands.push(Command::new(IconName::Store, t("Marketplace"), "App", on_shell(Box::new(|this, _, cx| this.show_page(Page::Marketplace, cx)))));
        commands.push(Command::new(IconName::Settings, t("Settings"), "App", on_shell(Box::new(|this, _, cx| this.show_page(Page::Settings, cx)))));
        for choice in Language::ALL {
            commands.push(Command::new(IconName::Languages, tf("Language: {}", &[&choice.native_name()]), "Settings", move |_, cx| {
                Settings::update(cx, |s| s.language = Some(choice))
            }));
        }
        for choice in Appearance::ALL {
            let icon = match choice {
                Appearance::System => IconName::SunMoon,
                Appearance::Light => IconName::Sun,
                Appearance::Dark => IconName::Moon,
            };
            commands.push(Command::new(icon, tf("Appearance: {}", &[&t(choice.label())]), "Settings", move |_, cx| {
                Settings::update(cx, |s| s.appearance = choice)
            }));
        }
        let snap = Settings::get(cx).snap_to_grid;
        commands.push(Command::new(IconName::LayoutGrid, if snap { t("Turn snapping to the grid off") } else { t("Turn snapping to the grid on") }, "Settings", move |_, cx| {
            Settings::update(cx, |s| s.snap_to_grid = !snap)
        }));
        commands.push(Command::new(IconName::LogOut, t("Quit XHarness"), "App", on_shell(Box::new(|this, window, cx| {
            this.quit(window, cx);
        }))).keys("Cmd+Q"));
        commands
    }
}
