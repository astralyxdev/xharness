//! XHarness: a canvas of terminals, editors, explorers and git for each
//! project, built on GPUI.
//!
//! - `app/`: the window: title bar, sidebar, pages (Home, Marketplace),
//!   dialogs.
//! - `canvas/`: a project's infinite canvas, which the windows sit on.
//! - `windows/`: the kinds of windows (mods): terminal, explorer, editor,
//!   git, lino.
//! - `services/`: everything without a UI: the project store, the session
//!   host that keeps shells running, git, lino, language servers.
//! - `ui/`: the design system: theme, icons, components, text input, menus.
//! - `platform/`: what GPUI doesn't do for us on macOS.

// The server-only build leaves out the app, and some helpers only it uses.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]
// A Windows release build of the app is a GUI program: no console window.
#![cfg_attr(all(windows, feature = "gui", not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(feature = "gui")]
mod app;
#[cfg(feature = "gui")]
mod canvas;
#[cfg(feature = "gui")]
mod platform;
mod services;
#[cfg(feature = "gui")]
mod ui;
#[cfg(feature = "gui")]
mod windows;

use services::server;

fn main() {
    // With the app (`gui`): the app, unless started as the server. Without
    // it, this binary is only the server (`xharness --listen …`).
    #[cfg(feature = "gui")]
    if !server::is_host_invocation() {
        gui::run();
        return;
    }
    // On a Mac, the app's server shows an icon in the menu bar (not for
    // `--stop` and `--help`, which return at once).
    #[cfg(all(feature = "gui", target_os = "macos"))]
    if !std::env::args().any(|arg| matches!(arg.as_str(), "--stop" | "--help" | "-h")) {
        platform::macos::run_server_with_tray(server::run_host);
    }
    if let Err(error) = server::run_host() {
        eprintln!("xharness server: {error}");
        std::process::exit(1);
    }
}

#[cfg(feature = "gui")]
mod gui {
use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds,
    WindowOptions, px, size,
};

use crate::app::{self, APP_TITLE, AppShell, Quit, RENAME_CONTEXT};
#[cfg(target_os = "macos")]
use crate::platform::macos;
use crate::{canvas, services, ui, windows};
use crate::ui::actions::{Cancel, Confirm};
#[cfg(target_os = "macos")]
use crate::ui::assets;
use crate::ui::assets::Assets;

pub fn run() {
    // For the build scripts: write the app icon (1024px PNG) and exit.
    #[cfg(target_os = "macos")]
    if let Some(at) = std::env::args().position(|arg| arg == "--write-app-icon") {
        let path = std::env::args().nth(at + 1).expect("--write-app-icon <file.png>");
        let png = macos::app_icon_png(assets::LOGO).expect("the logo renders");
        std::fs::write(path, png).expect("can write the icon");
        return;
    }

    Application::new().with_assets(Assets).run(|cx: &mut App| {
        #[cfg(target_os = "macos")]
        macos::set_app_icon(assets::LOGO);

        windows::Mods::init(cx);
        ui::text_input::bind_keys(cx);
        ui::context_menu::bind_keys(cx);
        ui::alert_dialog::bind_keys(cx);
        app::project_dialog::bind_keys(cx);
        windows::explorer::bind_keys(cx);
        windows::editor::bind_keys(cx);
        windows::git::bind_keys(cx);
        windows::terminal::bind_keys(cx);
        canvas::bind_keys(cx);
        cx.bind_keys([
            KeyBinding::new("enter", Confirm, Some(RENAME_CONTEXT)),
            KeyBinding::new("escape", Cancel, Some(RENAME_CONTEXT)),
            KeyBinding::new("secondary-q", Quit, None),
        ]);
        // Language servers are found on your shell's PATH; ask for it now,
        // off the main thread, so opening a file doesn't wait.
        cx.background_executor()
            .spawn(async {
                services::language_servers::user_path();
            })
            .detach();

        // Quitting (⌘Q, the menu, or closing the window) leaves terminals
        // running in the session host; reopening picks them up.
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![Menu {
            name: APP_TITLE.into(),
            items: vec![MenuItem::action(format!("Quit {APP_TITLE}"), Quit)],
        }]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1200.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(480.))),
                // Hide the native titlebar and title; content extends to the
                // top edge and only the traffic lights remain.
                titlebar: Some(TitlebarOptions {
                    title: Some(APP_TITLE.into()),
                    appears_transparent: true,
                    // Where macOS puts them: centred in the 32px title bar.
                    traffic_light_position: None,
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| AppShell::new(window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
}
