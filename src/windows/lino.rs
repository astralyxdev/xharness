//! The Lino window: what lino is doing in the project's folder (the index,
//! memory, how edits went, per-command latency, recent changes), refreshed
//! every few seconds while the window is open, with Start / Stop / Reindex.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gpui::{
    AnyElement, App, Context, Div, FocusHandle, FontWeight, Hsla, SharedString, Stateful,
    Task, Window, div, prelude::*, rems,
};

use crate::i18n::{t, tf, tn};
use crate::ui::assets::IconName;
use crate::windows::explorer::format_size;
use crate::services::lino::{self, Change, CommandStats, Snapshot, Stats, Status};
use crate::ui::theme::Theme;
use crate::ui;

const POLL_EVERY: Duration = Duration::from_secs(3);
const RECENT_CHANGES: usize = 8;
/// The calls this window makes to watch lino. The numbers it shows leave
/// them out: they're about what agents and tools asked of lino.
const OWN_COMMANDS: [&str; 2] = ["status", "stats"];

/// Outcomes of calls that did what was asked (lino's `internal/outcome`);
/// the rest (conflict, not_found, refused…) are counted as not ok.
const SUCCESSES: [&str; 5] = ["ok", "updated", "created", "empty", "truncated"];

/// How long something has been going, as [`lino::format_span`] says it
/// but in the app's language: `45s`, `12m`, `2h 5m`, `3d 4h`.
pub(crate) fn format_span(secs: u64) -> String {
    let (m, h, d) = (secs / 60, secs / 3600, secs / 86400);
    match secs {
        0..60 => tf("{}s", &[&secs]),
        60..3600 => tf("{}m", &[&m]),
        3600..86400 => tf("{}h {}m", &[&h, &(m % 60)]),
        _ => tf("{}d {}h", &[&d, &(h % 24)]),
    }
}

/// How long ago, by its largest unit: `just now`, `5m ago`, `3d ago`.
pub(crate) fn format_ago(secs: u64) -> String {
    let (m, h, d) = (secs / 60, secs / 3600, secs / 86400);
    let span = match secs {
        0..5 => return t("just now").to_string(),
        5..60 => tf("{}s", &[&secs]),
        60..3600 => tf("{}m", &[&m]),
        3600..86400 => tf("{}h", &[&h]),
        _ => tf("{}d", &[&d]),
    };
    tf("{} ago", &[&span])
}

/// [`format_ago`] for an RFC 3339 time, as lino writes them.
fn ago(at: &str, now: i64) -> Option<String> {
    Some(format_ago((now - lino::parse_time(at)?).max(0) as u64))
}

/// What a poll found.
struct Polled {
    snapshot: Snapshot,
    bundled: bool,
    /// The history's change count and its newest changes, when they're new.
    history: Option<(u64, Vec<Change>)>,
}

/// Asks lino what it's doing in `folder`. `None` means there's no lino.
/// The history is only fetched when lino's change count isn't `known`.
fn poll(folder: &Path, known: Option<u64>) -> Option<Polled> {
    let bundled = lino::available()?;
    let snapshot = lino::snapshot(folder)?;
    let history = match &snapshot {
        Snapshot::Running { stats: Some(stats), .. } if known != Some(stats.history.changes) => lino::history(folder, RECENT_CHANGES)
            .ok()
            .map(|changes| (stats.history.changes, changes)),
        _ => None,
    };
    Some(Polled { snapshot, bundled, history })
}

enum State {
    /// Before the first poll.
    Checking,
    /// No bundled lino for this platform, and none on `PATH`.
    Unavailable,
    Ready(Snapshot),
}

pub struct LinoView {
    folder: PathBuf,
    /// The server the folder is on (`None`: this machine).
    server: Option<crate::services::server::HostAddress>,
    state: State,
    bundled: bool,
    history: Vec<Change>,
    /// lino's change count when `history` was fetched.
    history_count: Option<u64>,
    /// What's being done ("Starting…"), and why the last action failed.
    busy: Option<&'static str>,
    error: Option<String>,
    focus_handle: FocusHandle,
    action: Option<Task<()>>,
    _poll: Task<()>,
}

impl LinoView {
    pub fn new(folder: PathBuf, server: Option<crate::services::server::HostAddress>, cx: &mut Context<Self>) -> Self {
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                let Ok((folder, known, server)) =
                    this.read_with(cx, |this, _| (this.folder.clone(), this.history_count, this.server.clone()))
                else {
                    break;
                };
                let polled = cx.background_executor().spawn(async move { crate::services::rpc::on(server.as_ref(), || poll(&folder, known)) }).await;
                if this.update(cx, |this, cx| this.apply(polled, cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(POLL_EVERY).await;
            }
        });
        Self {
            folder,
            server,
            state: State::Checking,
            bundled: false,
            history: Vec::new(),
            history_count: None,
            busy: None,
            error: None,
            focus_handle: cx.focus_handle(),
            action: None,
            _poll: poll_task,
        }
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// Whether lino's live process is up.
    pub fn running(&self) -> bool {
        matches!(self.state, State::Ready(Snapshot::Running { .. }))
    }

    /// A few words for the window's header.
    pub fn summary(&self) -> Option<String> {
        match &self.state {
            State::Ready(Snapshot::Running { status, .. }) => {
                Some(tf("up {} · {}", &[&format_span(status.uptime_s), &status.id]))
            }
            State::Ready(Snapshot::Stopped) => Some(t("stopped").to_string()),
            State::Ready(Snapshot::NotEnabled) => Some(t("not set up").to_string()),
            State::Ready(Snapshot::Failed(_)) => Some(t("error").to_string()),
            State::Unavailable => Some(t("not installed").to_string()),
            State::Checking => None,
        }
    }

    fn apply(&mut self, polled: Option<Polled>, cx: &mut Context<Self>) {
        match polled {
            None => self.state = State::Unavailable,
            Some(polled) => {
                if let Some((count, changes)) = polled.history {
                    self.history_count = Some(count);
                    self.history = changes;
                }
                self.bundled = polled.bundled;
                self.state = State::Ready(polled.snapshot);
            }
        }
        cx.notify();
    }

    /// Runs a lino command in the background, then looks again at once.
    fn act(&mut self, busy: &'static str, action: fn(&Path) -> Result<(), String>, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(busy);
        self.error = None;
        cx.notify();
        let folder = self.folder.clone();
        let server = self.server.clone();
        self.action = Some(cx.spawn(async move |this, cx| {
            let (result, polled) = cx
                .background_executor()
                .spawn(async move {
                    crate::services::rpc::on(server.as_ref(), || {
                        let result = action(&folder);
                        (result, poll(&folder, None))
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = None;
                this.error = result.err();
                this.apply(polled, cx);
            })
            .ok();
        }));
    }

    // Rendering ------------------------------------------------------------------

    fn render_toolbar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let (dot, label, detail) = match &self.state {
            State::Checking => (theme.muted_foreground, t("Checking…"), None),
            State::Unavailable => (theme.tints.destructive.base, t("Not installed"), None),
            State::Ready(Snapshot::Running { status, .. }) => (
                theme.tints.green.base,
                t("Running"),
                Some(tf("{} · pid {} · up {}", &[&status.id, &status.pid, &format_span(status.uptime_s)])),
            ),
            State::Ready(Snapshot::Stopped) => (theme.muted_foreground, t("Stopped"), None),
            State::Ready(Snapshot::NotEnabled) => (theme.tints.amber.base, t("Not set up"), None),
            State::Ready(Snapshot::Failed(_)) => (theme.tints.destructive.base, t("Error"), None),
        };

        let buttons: Vec<AnyElement> = if let Some(busy) = self.busy {
            vec![ui::compact_button("lino-busy", None, busy, false, theme).opacity(0.6).into_any_element()]
        } else {
            match &self.state {
                State::Ready(Snapshot::Running { .. }) => vec![
                    ui::compact_button("lino-reindex", Some(IconName::RefreshCw), t("Reindex"), false, theme)
                        .on_click(cx.listener(|this, _, _, cx| this.act(t("Reindexing…"), lino::reindex, cx)))
                        .into_any_element(),
                    ui::compact_button("lino-stop", Some(IconName::Square), t("Stop"), false, theme)
                        .on_click(cx.listener(|this, _, _, cx| this.act(t("Stopping…"), lino::stop, cx)))
                        .into_any_element(),
                ],
                State::Ready(Snapshot::Stopped | Snapshot::Failed(_)) => vec![
                    ui::compact_button("lino-start", Some(IconName::Play), t("Start"), false, theme)
                        .on_click(cx.listener(|this, _, _, cx| this.act(t("Starting…"), lino::start, cx)))
                        .into_any_element(),
                ],
                _ => Vec::new(),
            }
        };

        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .h_10()
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .child(dot_element(dot))
            .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(label))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(rems(0.75))
                    .text_color(theme.muted_foreground)
                    .children(detail),
            )
            .children(buttons)
    }

    /// The body when there's nothing to show yet: a message, maybe a button.
    fn render_message(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let (title, text, button): (&str, String, Option<Stateful<Div>>) = match &self.state {
            State::Checking => (t("Checking lino…"), String::new(), None),
            State::Unavailable => (
                t("lino isn't installed"),
                t("This build of xharness doesn't bundle lino for this platform, and there's no lino on PATH.").to_string(),
                None,
            ),
            State::Ready(Snapshot::NotEnabled) => (
                t("lino isn't set up here"),
                t("lino indexes this folder so agents can read, search and edit it in fewer tokens, \
                   with every change recorded. Setting it up adds .lino/ and a .linoignore to the folder.")
                .to_string(),
                Some(
                    ui::compact_button("lino-enable", Some(IconName::Play), t("Set up lino"), true, theme)
                        .on_click(cx.listener(|this, _, _, cx| this.act(t("Setting up…"), lino::enable, cx))),
                ),
            ),
            State::Ready(Snapshot::Stopped) => (
                t("lino isn't running"),
                t("Its live process stops after a while idle. Agents' lino commands start it again, or start it now.").to_string(),
                Some(
                    ui::compact_button("lino-start-body", Some(IconName::Play), t("Start lino"), true, theme)
                        .on_click(cx.listener(|this, _, _, cx| this.act(t("Starting…"), lino::start, cx))),
                ),
            ),
            State::Ready(Snapshot::Failed(error)) => (t("lino didn't answer"), error.clone(), None),
            State::Ready(Snapshot::Running { .. }) => unreachable!("rendered by render_running"),
        };
        let button = if self.busy.is_some() { None } else { button };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .px_8()
            .text_center()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
            .when(!text.is_empty(), |el| {
                el.child(div().max_w(rems(26.)).text_color(theme.muted_foreground).child(text))
            })
            .children(button.map(|button| div().mt_2().child(button)))
            .into_any_element()
    }

    fn render_running(&self, status: &Status, stats: Option<&Stats>, theme: &Theme) -> AnyElement {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let default = Stats::default();
        let stats = stats.unwrap_or(&default);
        let mut commands: Vec<&CommandStats> = stats
            .commands
            .iter()
            .filter(|c| c.count > 0 && !OWN_COMMANDS.contains(&c.name.as_str()))
            .collect();
        commands.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        let calls: u64 = commands.iter().map(|c| c.count).sum();

        let tiles = [
            (t("Files"), lino::group_digits(status.totals.files), None),
            (t("Lines"), lino::group_digits(status.totals.lines), None),
            (t("Changes"), lino::group_digits(stats.history.changes), None),
            (t("Calls"), lino::group_digits(calls), None),
            (
                t("Memory"),
                format_size(stats.process.peak_rss_bytes),
                Some(tf("heap {}", &[&format_size(stats.process.heap_bytes)])),
            ),
            (
                t("On disk"),
                format_size(stats.index.db_bytes + stats.history.db_bytes),
                Some(tf("history {}", &[&format_size(stats.history.db_bytes)])),
            ),
        ];
        let mut tiles: Vec<Div> = tiles
            .into_iter()
            .map(|(label, value, detail)| tile(label, value, detail, theme))
            .collect();
        let second_row = tiles.split_off(3);

        let last_sync = status.last_reconcile.as_ref().map(|r| {
            let when = ago(&r.at, now).unwrap_or_default();
            tf("{} · {} ms · +{} ~{} −{} ↻{}", &[&when, &r.duration_ms, &r.added, &r.modified, &r.removed, &r.moved])
        });
        let rate = |outcome: &str| {
            stats.rates.iter().find(|r| r.outcome == outcome).map_or("–".to_string(), |r| {
                if r.of == 0 {
                    "–".to_string()
                } else {
                    tf("{}% · {} of {}", &[&format!("{:.1}", r.count as f64 * 100. / r.of as f64), &r.count, &r.of])
                }
            })
        };
        let reindex = if stats.reindex.count == 0 {
            t("none yet").to_string()
        } else {
            format!(
                "{} · p50 {} · p95 {}",
                stats.reindex.count,
                lino::format_micros(stats.reindex.p50_us),
                lino::format_micros(stats.reindex.p95_us)
            )
        };

        div()
            .id("lino-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().flex().gap_2().children(tiles))
                            .child(div().flex().gap_2().children(second_row)),
                    )
                    .child(
                        section(t("Index"), theme)
                            .child(row(t("Watcher"), status.watcher.clone(), theme))
                            .child(row(t("Waiting to index"), tn(status.pending as usize, "{} file", "{} files"), theme))
                            .when(status.totals.binary > 0, |el| {
                                el.child(row(t("Binary files"), lino::group_digits(status.totals.binary), theme))
                            })
                            .children(last_sync.map(|text| row(t("Last sync"), text, theme))),
                    )
                    .child(
                        section(t("Edits"), theme)
                            .child(row(t("Conflicts"), rate("conflict"), theme))
                            .child(row(t("Anchor mismatches"), rate("anchor_mismatch"), theme))
                            .child(row(t("Changed outside lino"), lino::group_digits(stats.external), theme))
                            .child(row(t("Reindexes"), reindex, theme)),
                    )
                    .child(section(t("Commands"), theme).child(if commands.is_empty() {
                        empty_note(t("No calls yet. Agents use lino from a terminal; `lino help --agent` prints the instructions to give them."), theme)
                    } else {
                        commands_table(&commands, theme)
                    }))
                    .child(section(t("Recent changes"), theme).child(if self.history.is_empty() {
                        empty_note(t("No changes yet."), theme)
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .children(self.history.iter().map(|change| change_row(change, now, theme)))
                            .into_any_element()
                    })),
            )
            .into_any_element()
    }
}

impl Render for LinoView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);
        let body = match &self.state {
            State::Ready(Snapshot::Running { status, stats }) => self.render_running(status, stats.as_ref(), theme),
            _ => self.render_message(theme, cx),
        };
        let source = if self.bundled {
            tf("lino {} · bundled", &[&lino::VERSION])
        } else {
            t("lino from PATH").to_string()
        };

        div()
            .id("lino")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .text_size(rems(0.8125))
            .line_height(rems(1.25))
            .text_color(theme.foreground)
            // The body scrolls; the canvas shouldn't pan along with it.
            .on_scroll_wheel(cx.listener(|this, _, window, cx| {
                if this.contains_focus(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(self.render_toolbar(theme, cx))
            .children(self.error.clone().map(|error| {
                div()
                    .flex_shrink_0()
                    .px_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(rems(0.75))
                    .text_color(theme.tints.destructive.soft_foreground)
                    .bg(theme.tints.destructive.base.opacity(0.08))
                    .child(error)
            }))
            .child(body)
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .h_6()
                    .px_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(rems(0.6875))
                    .text_color(theme.muted_foreground)
                    .child(div().flex_shrink_0().child(source))
                    .child(div().flex_1().min_w_0().truncate().child(ui::display_path(&self.folder))),
            )
    }
}

// Pieces ---------------------------------------------------------------------------

fn dot_element(color: Hsla) -> Div {
    div().flex_shrink_0().size(rems(0.4375)).rounded_full().bg(color)
}

/// A number with its label: border rounded-lg p-2.5.
fn tile(label: &'static str, value: String, detail: Option<String>, theme: &Theme) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_0p5()
        .p_2p5()
        .rounded(rems(0.625))
        .border_1()
        .border_color(theme.border)
        .bg(theme.card)
        .child(div().text_size(rems(0.6875)).text_color(theme.muted_foreground).child(label))
        .child(
            div()
                .truncate()
                .text_size(rems(1.125))
                .line_height(rems(1.5))
                .font_weight(FontWeight::SEMIBOLD)
                .child(value),
        )
        .children(detail.map(|detail| {
            div()
                .truncate()
                .text_size(rems(0.6875))
                .text_color(theme.muted_foreground)
                .child(detail)
        }))
}

fn section(title: &'static str, theme: &Theme) -> Div {
    div().flex().flex_col().gap_1().child(
        div()
            .pb_1()
            .text_size(rems(0.6875))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.muted_foreground)
            .child(title),
    )
}

fn row(label: &'static str, value: String, theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(div().w(rems(10.)).flex_shrink_0().text_color(theme.muted_foreground).child(label))
        .child(div().flex_1().min_w_0().truncate().child(value))
}

fn empty_note(text: &'static str, theme: &Theme) -> AnyElement {
    div().text_color(theme.muted_foreground).child(text).into_any_element()
}

/// Calls per command, slowest last: name, calls, p50, p95, max.
fn commands_table(commands: &[&CommandStats], theme: &Theme) -> AnyElement {
    let cell = |text: String| div().w(rems(4.5)).flex_shrink_0().flex().justify_end().child(text);
    let header = div()
        .flex()
        .items_center()
        .pb_1()
        .text_size(rems(0.6875))
        .text_color(theme.muted_foreground)
        .child(div().flex_1().child(t("Command")))
        .child(cell(t("Calls").into()))
        .child(cell("p50".into()))
        .child(cell("p95".into()))
        .child(cell(t("Max").into()));
    div()
        .flex()
        .flex_col()
        .child(header)
        .children(commands.iter().map(|command| {
            let failures: u64 = command
                .outcomes
                .iter()
                .filter(|(outcome, _)| !SUCCESSES.contains(&outcome.as_str()))
                .map(|(_, count)| count)
                .sum();
            div()
                .flex()
                .items_center()
                .h_6()
                .border_t_1()
                .border_color(theme.border.opacity(0.6))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(command.name.clone())
                        .when(failures > 0, |el| {
                            el.child(
                                div()
                                    .text_size(rems(0.6875))
                                    .text_color(theme.tints.amber.soft_foreground)
                                    .child(tf("{} not ok", &[&failures])),
                            )
                        }),
                )
                .child(cell(lino::group_digits(command.count)))
                .child(cell(lino::format_micros(command.p50_us)))
                .child(cell(lino::format_micros(command.p95_us)))
                .child(cell(lino::format_micros(command.max_us)))
        }))
        .into_any_element()
}

fn change_row(change: &Change, now: i64, theme: &Theme) -> impl IntoElement {
    let when = ago(&change.time, now).unwrap_or_default();
    let meta = match &change.author {
        Some(author) if !author.is_empty() => format!("{author} · {when}"),
        _ => when,
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .h_6()
        .border_t_1()
        .border_color(theme.border.opacity(0.6))
        .child(
            div()
                .w(rems(2.5))
                .flex_shrink_0()
                .text_size(rems(0.6875))
                .text_color(theme.muted_foreground)
                .child(format!("#{}", change.id)),
        )
        .child(
            div()
                .flex_shrink_0()
                .px_1p5()
                .rounded(rems(0.25))
                .bg(theme.secondary)
                .text_size(rems(0.6875))
                .line_height(rems(1.))
                .child(change.op.clone()),
        )
        .child(div().flex_1().min_w_0().truncate().child(SharedString::from(change.path.clone())))
        .child(
            div()
                .flex_shrink_0()
                .text_size(rems(0.6875))
                .text_color(theme.muted_foreground)
                .child(meta),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_as_how_long_ago() {
        assert_eq!(ago("2026-11-01T21:08:34+03:00", 1_793_556_514 + 300).as_deref(), Some("5m ago"));
        assert_eq!(ago("2026-11-01T21:08:34+03:00", 1_793_556_514 + 2).as_deref(), Some("just now"));
        assert_eq!(format_ago(3 * 86_400 + 5), "3d ago");
        assert_eq!(format_span(2 * 3600 + 5 * 60), "2h 5m");
    }
}
