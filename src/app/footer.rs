//! The footer, like VS Code's status bar: the server the selected project is
//! on (this Mac's, or a remote one) and how it's doing. This Mac's server
//! starts and stops here; a remote one can be retried. And what a project
//! shows instead of its canvas while its server can't be reached.

use super::*;
use servers::Status;

const FOOTER_HEIGHT: f32 = 24.;

/// `color`, a little lighter.
fn lighten(color: gpui::Hsla, by: f32) -> gpui::Hsla {
    gpui::Hsla { l: (color.l + by).min(1.), ..color }
}

fn span(secs: u64) -> String {
    lino::format_span(secs)
}

impl AppShell {
    /// The selected project's server (`None`: this Mac's, also for pages).
    pub(super) fn selected_server(&self, cx: &App) -> Option<server::ServerKey> {
        match self.selection {
            Selection::Project(id) => self.project(id, cx).and_then(|p| p.remote.clone()),
            Selection::Page(_) => None,
        }
    }

    pub(super) fn render_footer(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let key = self.selected_server(cx);
        let status = self.servers.read(cx).status(key.as_ref());
        let fg = theme.sidebar_foreground;
        let tints = &theme.sidebar_tints;
        let (dot, mut parts): (gpui::Hsla, Vec<String>) = match (&status, &key) {
            (Status::Connected { info, .. }, None) => (
                tints.green.base,
                vec![
                    "Server running".into(),
                    format!("{} terminal{}", info.sessions, if info.sessions == 1 { "" } else { "s" }),
                    format!("up {}", span(info.uptime_s)),
                ],
            ),
            (Status::Connected { latency, info }, Some(key)) => (
                tints.green.base,
                vec![
                    key.address(),
                    format!("{} ms", latency.as_millis().max(1)),
                    info.os.clone(),
                    format!("{} terminal{}", info.sessions, if info.sessions == 1 { "" } else { "s" }),
                    format!("up {}", span(info.uptime_s)),
                ],
            ),
            (Status::Offline(_), None) => (fg.opacity(0.4), vec!["Server stopped".into()]),
            (Status::Offline(error), Some(key)) => (tints.destructive.base, vec![key.address(), format!("not connected: {error}")]),
            (Status::Starting, _) => (tints.amber.base, vec!["Starting the server…".into()]),
            (Status::Stopping, _) => (tints.amber.base, vec!["Stopping the server…".into()]),
            (Status::Checking, None) => (tints.amber.base, vec!["Checking the server…".into()]),
            (Status::Checking, Some(key)) => (tints.amber.base, vec![key.address(), "connecting…".into()]),
        };
        let (icon, name) = match &key {
            Some(key) => (IconName::Globe, key.name.clone()),
            None => (IconName::HardDrive, "This Mac".to_string()),
        };
        parts.insert(0, name);
        let action: Option<(&'static str, &'static str)> = match (&status, &key) {
            (Status::Connected { .. }, None) => Some(("footer-stop", "Stop")),
            (Status::Offline(_), None) => Some(("footer-start", "Start")),
            (Status::Offline(_), Some(_)) => Some(("footer-retry", "Retry")),
            _ => None,
        };
        let folder = match self.selection {
            Selection::Project(id) => self
                .project(id, cx)
                .and_then(|p| p.folder.as_ref())
                .map(|folder| if key.is_some() { folder.display().to_string() } else { ui::display_path(folder) }),
            Selection::Page(_) => None,
        };
        let button_hover = fg.opacity(0.14);

        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .h(px(FOOTER_HEIGHT))
            .px(px(GUTTER + 6.))
            .bg(lighten(theme.sidebar, 0.05))
            .text_size(px(12.))
            .line_height(px(16.))
            .text_color(fg.opacity(0.75))
            .child(ui::icon(icon).size_3p5().text_color(fg.opacity(0.75)))
            .child(ui::status_dot(dot))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(parts.join("  ·  ")),
            )
            .children(action.map(|(id, label)| {
                let key = key.clone();
                div()
                    .id(id)
                    .flex_shrink_0()
                    .px(px(8.))
                    .rounded(px(4.))
                    .text_color(fg)
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(move |style| style.bg(button_hover))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| match id {
                        "footer-stop" => this.confirm_stop_server(window, cx),
                        "footer-start" => this.servers.update(cx, |s, cx| s.start_local(cx)),
                        _ => this.servers.update(cx, |s, cx| s.check(key.clone(), cx)),
                    }))
            }))
            .children(folder.map(|folder| div().flex_shrink_0().max_w(px(360.)).truncate().text_color(fg.opacity(0.5)).child(folder)))
    }

    /// Asks before stopping this Mac's server: it ends every shell in it.
    pub(super) fn confirm_stop_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let alert = cx.new(|cx| {
            AlertDialog::destructive(
                "Stop the server?",
                "It ends every terminal in this Mac's projects, and anything running in them. Projects on this Mac can't open again until you start it.",
                "Stop server",
                window,
                cx,
            )
        });
        let subscription = cx.subscribe(&alert, |this, alert, event, cx| {
            if let AlertDialogEvent::Confirm = event {
                this.servers.update(cx, |servers, cx| servers.stop_local(cx));
            }
            this.close_modal(alert.entity_id(), cx);
        });
        self.modal = Some(Modal {
            view: alert.into(),
            _subscription: subscription,
        });
        cx.notify();
    }

    /// What a project shows while its server can't be reached.
    pub(super) fn render_offline(&self, project: &Project, status: &Status, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let key = project.remote.clone();
        let (icon, title, text, button): (IconName, String, String, Option<Button>) = match (&key, status) {
            (None, Status::Starting) => (IconName::HardDrive, "Starting the server…".into(), String::new(), None),
            (None, _) => (
                IconName::HardDrive,
                "The server isn't running".into(),
                format!(
                    "Terminals, files and git for projects on this Mac run in XHarness's server, so they keep going when the app quits. Start it to open {}.",
                    project.name
                ),
                Some(
                    Button::new("offline-start", "Start server")
                        .size(ButtonSize::Large)
                        .on_click(cx.listener(|this, _, _, cx| this.servers.update(cx, |s, cx| s.start_local(cx)))),
                ),
            ),
            (Some(key), Status::Checking) => (IconName::Globe, format!("Connecting to {}…", key.name), key.address(), None),
            (Some(key), status) => {
                let error = match status {
                    Status::Offline(error) => error.clone(),
                    _ => "not connected".into(),
                };
                let retry_key = key.clone();
                (
                    IconName::Globe,
                    format!("Can't reach {}", key.name),
                    format!(
                        "{} — {error}. Is `xharness --server --listen …` running there, with port {} open to this Mac?",
                        key.address(),
                        key.port
                    ),
                    Some(
                        Button::new("offline-retry", "Retry")
                            .size(ButtonSize::Large)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let key = retry_key.clone();
                                this.servers.update(cx, |s, cx| s.check(Some(key), cx))
                            })),
                    ),
                )
            }
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .px(px(48.))
            .text_center()
            .child(ui::icon(icon).size_8().text_color(theme.muted_foreground))
            .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child(title))
            .when(!text.is_empty(), |el| {
                el.child(div().max_w(px(460.)).text_size(px(14.)).line_height(relative(1.5)).text_color(theme.muted_foreground).child(text))
            })
            .children(button.map(|button| div().mt_2().child(button)))
            .into_any_element()
    }
}
