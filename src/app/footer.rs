//! The footer, like VS Code's status bar. On the left, the server the
//! selected project is on (this Mac's, or a remote one) and how it's doing:
//! this Mac's starts and stops here, a remote one can be retried. On the
//! right, the project: its git branch and changes (click: the Git window),
//! commands running in its terminals, the active window, its windows and
//! marks, the zoom (click: fit), and the language (click: switch).
//!
//! And what a project shows instead of its canvas while its server can't be
//! reached.

use std::time::Duration;

use super::settings::Settings;
use super::*;
use crate::i18n::{Language, t, tf, tn};
use crate::services::git;
use servers::Status;

const FOOTER_HEIGHT: f32 = 26.;
/// How often the project's branch and changes are read.
const GIT_EVERY: Duration = Duration::from_secs(4);

/// The selected project's repository, as the footer shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct GitSummary {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changes: usize,
}

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

    /// Reads the selected project's branch and changes every few seconds,
    /// for the footer.
    pub(super) fn watch_git(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let target = this
                    .read_with(cx, |this, cx| match this.selection {
                        Selection::Project(id) => this.project(id, cx).and_then(|p| {
                            let connected = this.servers.read(cx).status(p.remote.as_ref()).is_connected();
                            connected.then(|| (id, p.folder.clone(), p.server()))
                        }),
                        Selection::Page(_) => None,
                    })
                    .ok();
                let Some(target) = target else {
                    break;
                };
                let summary = match target.clone() {
                    Some((id, Some(folder), server)) => {
                        let summary = cx
                            .background_executor()
                            .spawn(async move {
                                crate::services::rpc::on(server.as_ref(), || {
                                    let repo = git::discover(&folder)?;
                                    let status = git::status(&repo).ok()?;
                                    Some(GitSummary {
                                        branch: status.branch,
                                        ahead: status.ahead,
                                        behind: status.behind,
                                        changes: status.changes.len(),
                                    })
                                })
                            })
                            .await;
                        Some((id, summary))
                    }
                    _ => None,
                };
                let updated = this.update(cx, |this, cx| {
                    let next = summary.and_then(|(id, summary)| summary.map(|summary| (id, summary)));
                    if this.git_summary != next {
                        this.git_summary = next;
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
                cx.background_executor().timer(GIT_EVERY).await;
            }
        })
    }

    pub(super) fn render_footer(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let key = self.selected_server(cx);
        let status = self.servers.read(cx).status(key.as_ref());
        let fg = theme.sidebar_foreground;
        let tints = &theme.sidebar_tints;
        let (dot, state): (gpui::Hsla, String) = match (&status, &key) {
            (Status::Connected { .. }, None) => (tints.green.base, t("Server running").into()),
            (Status::Connected { latency, .. }, Some(key)) => {
                (tints.green.base, format!("{} · {} ms", key.address(), latency.as_millis().max(1)))
            }
            (Status::Offline(_), None) => (fg.opacity(0.4), t("Server stopped").into()),
            (Status::Offline(error), Some(key)) => (tints.destructive.base, format!("{} · {}", key.address(), tf("not connected: {}", &[error]))),
            (Status::Starting, _) => (tints.amber.base, t("Starting the server…").into()),
            (Status::Stopping, _) => (tints.amber.base, t("Stopping the server…").into()),
            (Status::Checking, None) => (tints.amber.base, t("Checking the server…").into()),
            (Status::Checking, Some(key)) => (tints.amber.base, format!("{} · {}", key.address(), t("connecting…"))),
        };
        let (icon, name) = match &key {
            Some(key) => (IconName::Globe, key.name.clone()),
            None => (IconName::HardDrive, t("This Mac").to_string()),
        };
        let server_details: Option<String> = match &status {
            Status::Connected { info, .. } => Some(format!(
                "{} · {} · {}",
                tn(info.sessions, "{} shell", "{} shells"),
                tn(info.clients, "{} app connected", "{} apps connected"),
                tf("up {}", &[&span(info.uptime_s)])
            )),
            _ => None,
        };
        let action: Option<(&'static str, &'static str)> = match (&status, &key) {
            (Status::Connected { .. }, None) => Some(("footer-stop", "Stop")),
            (Status::Offline(_), None) => Some(("footer-start", "Start")),
            (Status::Offline(_), Some(_)) => Some(("footer-retry", "Retry")),
            _ => None,
        };
        let project = match self.selection {
            Selection::Project(id) => self.project(id, cx).map(|p| (id, p)),
            Selection::Page(_) => None,
        };
        let stats = project
            .and_then(|(id, _)| self.canvases.get(&id))
            .map(|(canvas, _)| canvas.read(cx).stats(cx));
        let git = project.and_then(|(id, _)| self.git_summary.as_ref().filter(|(summary_id, _)| *summary_id == id).map(|(_, git)| git.clone()));
        let language = Settings::get(cx).language();
        let hover = fg.opacity(0.12);
        let muted = fg.opacity(0.62);

        // One segment: an icon and text, clickable when `id` is given.
        let segment = |id: &'static str, icon: Option<IconName>, text: String, color: gpui::Hsla| {
            div()
                .id(id)
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(5.))
                .h(px(20.))
                .px(px(6.))
                .rounded(px(5.))
                .text_color(color)
                .hover(move |style| style.bg(hover))
                .children(icon.map(|icon| ui::icon(icon).size_3p5().text_color(color)))
                .child(text)
        };
        let divider = || div().flex_shrink_0().w_px().h(px(12.)).mx(px(2.)).bg(fg.opacity(0.16));

        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(2.))
            .h(px(FOOTER_HEIGHT))
            .px(px(GUTTER))
            .border_t_1()
            .border_color(fg.opacity(0.08))
            .bg(lighten(theme.sidebar, 0.04))
            .text_size(px(12.))
            .line_height(px(16.))
            .text_color(muted)
            .overflow_hidden()
            // The server.
            .child(
                segment("footer-server", Some(icon), name, fg.opacity(0.85))
                    .child(ui::status_dot(dot))
                    .child(div().text_color(muted).child(state)),
            )
            .children(server_details.map(|details| segment("footer-server-details", Some(IconName::Cpu), details, muted)))
            .children(action.map(|(id, label)| {
                let key = key.clone();
                div()
                    .id(id)
                    .flex_shrink_0()
                    .h(px(18.))
                    .px(px(8.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(fg.opacity(0.18))
                    .text_color(fg)
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(t(label))
                    .on_click(cx.listener(move |this, _, window, cx| match id {
                        "footer-stop" => this.confirm_stop_server(window, cx),
                        "footer-start" => this.servers.update(cx, |s, cx| s.start_local(cx)),
                        _ => this.servers.update(cx, |s, cx| s.check(key.clone(), cx)),
                    }))
            }))
            .child(div().flex_1().min_w(px(8.)))
            // The project.
            .when_some(git, |footer, git| {
                let mut text = git.branch.clone().unwrap_or_else(|| t("no branch").to_string());
                if git.ahead > 0 {
                    text.push_str(&format!(" ↑{}", git.ahead));
                }
                if git.behind > 0 {
                    text.push_str(&format!(" ↓{}", git.behind));
                }
                let project = project.map(|(id, _)| id);
                footer
                    .child(
                        segment("footer-branch", Some(IconName::GitBranch), text, fg.opacity(0.85))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(project) = project {
                                    this.open_kind(project, ItemKind::Git, window, cx);
                                }
                            })),
                    )
                    .child(segment(
                        "footer-changes",
                        Some(IconName::GitCommit),
                        if git.changes == 0 { t("no changes").to_string() } else { tn(git.changes, "{} change", "{} changes") },
                        if git.changes == 0 { muted } else { tints.amber.base },
                    ))
                    .child(divider())
            })
            .when_some(stats, |footer, stats| {
                footer
                    .when(stats.terminals > 0, |footer| {
                        let color = if stats.running > 0 { tints.blue.base } else { muted };
                        let text = if stats.running > 0 {
                            tn(stats.running, "{} command running", "{} commands running")
                        } else {
                            tn(stats.terminals, "{} terminal", "{} terminals")
                        };
                        footer.child(segment("footer-running", Some(IconName::SquareTerminal), text, color))
                    })
                    .when_some(stats.active.clone(), |footer, (kind, title)| {
                        footer.child(segment("footer-active", Some(kind.icon()), title, fg.opacity(0.85)).max_w(px(220.)).truncate())
                    })
                    .child(segment(
                        "footer-layout",
                        Some(IconName::LayoutGrid),
                        format!("{} · {}", tn(stats.windows, "{} window", "{} windows"), tn(stats.marks, "{} mark", "{} marks")),
                        muted,
                    ))
                    .child(
                        segment("footer-zoom", Some(IconName::Search), format!("{}%", (stats.zoom * 100.).round()), muted)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Selection::Project(id) = this.selection
                                    && let Some((canvas, _)) = this.canvases.get(&id)
                                {
                                    canvas.update(cx, |canvas, cx| canvas.run(crate::canvas::CanvasCommand::ZoomToFit, window, cx));
                                }
                            })),
                    )
                    .child(divider())
            })
            .child(
                segment(
                    "footer-language",
                    Some(IconName::Languages),
                    match language {
                        Language::English => "EN".into(),
                        Language::Russian => "RU".into(),
                    },
                    muted,
                )
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let next = match language {
                        Language::English => Language::Russian,
                        Language::Russian => Language::English,
                    };
                    Settings::update(cx, |s| s.language = Some(next));
                }),
            )
    }

    /// Asks before stopping this Mac's server: it ends every shell in it.
    pub(super) fn confirm_stop_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let alert = cx.new(|cx| {
            AlertDialog::destructive(
                t("Stop the server?"),
                t("It ends every terminal in this Mac's projects, and anything running in them. Projects on this Mac can't open again until you start it."),
                t("Stop server"),
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
            (None, Status::Starting) => (IconName::HardDrive, t("Starting the server…").into(), String::new(), None),
            (None, _) => (
                IconName::HardDrive,
                t("The server isn't running").into(),
                tf(
                    "Terminals, files and git for projects on this Mac run in XHarness's server, so they keep going when the app quits. Start it to open {}.",
                    &[&project.name],
                ),
                Some(
                    Button::new("offline-start", t("Start server"))
                        .size(ButtonSize::Large)
                        .on_click(cx.listener(|this, _, _, cx| this.servers.update(cx, |s, cx| s.start_local(cx)))),
                ),
            ),
            (Some(key), Status::Checking) => (IconName::Globe, tf("Connecting to {}…", &[&key.name]), key.address(), None),
            (Some(key), status) => {
                let error = match status {
                    Status::Offline(error) => error.clone(),
                    _ => t("not connected").into(),
                };
                let retry_key = key.clone();
                (
                    IconName::Globe,
                    tf("Can't reach {}", &[&key.name]),
                    tf(
                        "{} — {}. Is `xharness --server --listen …` running there, with port {} open to this Mac?",
                        &[&key.address(), &error, &key.port],
                    ),
                    Some(
                        Button::new("offline-retry", t("Retry"))
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
