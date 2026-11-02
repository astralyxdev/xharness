//! The command palette (⌘K): everything the app can do, found by typing.
//! The shell builds the list each time it opens (see `AppShell::commands`).

use std::rc::Rc;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, ScrollStrategy, SharedString,
    Subscription, UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};

use crate::i18n::t;
use crate::ui::actions::{Cancel, Confirm, SelectNext, SelectPrevious};
use crate::ui::assets::IconName;
use crate::ui::text_input::{TextInput, TextInputEvent};
use crate::ui::theme::{Theme, radius};
use crate::ui::{self};

actions!(palette, [OpenCommandPalette]);

const KEY_CONTEXT: &str = "CommandPalette";

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-k", OpenCommandPalette, None),
        KeyBinding::new("secondary-shift-p", OpenCommandPalette, None),
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("enter", Confirm, context),
        KeyBinding::new("escape", Cancel, context),
    ]);
}

/// Something the palette can run.
#[derive(Clone)]
pub struct Command {
    pub icon: IconName,
    pub label: SharedString,
    /// What it belongs to ("Canvas", "Go to"…), shown at the right.
    pub group: &'static str,
    /// Its shortcut, in `ui::shortcut` form ("Cmd+T").
    pub keys: Option<&'static str>,
    pub run: Rc<dyn Fn(&mut Window, &mut App)>,
}

impl Command {
    pub fn new(icon: IconName, label: impl Into<SharedString>, group: &'static str, run: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Self {
            icon,
            label: label.into(),
            group,
            keys: None,
            run: Rc::new(run),
        }
    }

    pub fn keys(mut self, keys: &'static str) -> Self {
        self.keys = Some(keys);
        self
    }
}

pub enum PaletteEvent {
    /// Close the palette, then run this.
    Run(Rc<dyn Fn(&mut Window, &mut App)>),
    Dismiss,
}

pub struct CommandPalette {
    input: Entity<TextInput>,
    commands: Vec<Command>,
    /// Indices into `commands` that match, best first.
    matches: Vec<usize>,
    selected: usize,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// How well `query` matches `label` (higher is better), or `None`. Every
/// word of the query has to appear, in order or not; whole-word and prefix
/// matches rank first.
pub fn score(label: &str, query: &str) -> Option<i32> {
    let label = label.to_lowercase();
    let mut total = 0;
    for word in query.to_lowercase().split_whitespace() {
        let at = label.find(word)?;
        let starts_word = at == 0 || !label[..at].chars().next_back().is_some_and(char::is_alphanumeric);
        total += if at == 0 {
            30
        } else if starts_word {
            20
        } else {
            8
        };
        total -= (at as i32).min(20) / 4;
    }
    Some(total)
}

impl CommandPalette {
    pub fn new(commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new(t("Type a command or search…"), cx).inline());
        window.focus(&input.focus_handle(cx));
        let subscription = cx.subscribe(&input, |this, _, _: &TextInputEvent, cx| this.filter(cx));
        let mut this = Self {
            input,
            matches: (0..commands.len()).collect(),
            commands,
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscription: subscription,
        };
        this.filter(cx);
        this
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text().trim().to_string();
        let mut scored: Vec<(i32, usize)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(ix, command)| {
                let haystack = format!("{} {}", command.label, t(command.group));
                score(&haystack, &query).map(|score| (score, ix))
            })
            .collect();
        if !query.is_empty() {
            // Stable: equally good matches keep the list's order.
            scored.sort_by_key(|(score, _)| -score);
        }
        self.matches = scored.into_iter().map(|(_, ix)| ix).collect();
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
            self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
            cx.notify();
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
            self.scroll.scroll_to_item(self.selected, ScrollStrategy::Center);
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.run(self.selected, cx);
    }

    fn run(&mut self, position: usize, cx: &mut Context<Self>) {
        if let Some(command) = self.matches.get(position).and_then(|&ix| self.commands.get(ix)) {
            cx.emit(PaletteEvent::Run(command.run.clone()));
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaletteEvent::Dismiss);
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx).clone();
        let rows = self.matches.len();
        let selected = self.selected;
        let entity = cx.entity();
        // At most this many rows show; the rest scroll.
        const ROW: f32 = 38.;
        const VISIBLE: usize = 9;
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(76.))
            .bg(gpui::black().opacity(if theme.mode == crate::ui::theme::Mode::Dark { 0.45 } else { 0.18 }))
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismiss)))
            .child(
                div()
                    .id("command-palette")
                    .key_context(KEY_CONTEXT)
                    .track_focus(&self.focus_handle)
                    .on_action(cx.listener(Self::select_next))
                    .on_action(cx.listener(Self::select_previous))
                    .on_action(cx.listener(Self::confirm))
                    .on_action(cx.listener(Self::cancel))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .flex()
                    .flex_col()
                    .w(px(640.))
                    .overflow_hidden()
                    .rounded(radius::XL2)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .shadow(vec![
                        gpui::BoxShadow {
                            color: gpui::black().opacity(0.10),
                            offset: gpui::point(px(0.), px(2.)),
                            blur_radius: px(6.),
                            spread_radius: px(0.),
                        },
                        gpui::BoxShadow {
                            color: gpui::black().opacity(0.28),
                            offset: gpui::point(px(0.), px(24.)),
                            blur_radius: px(60.),
                            spread_radius: px(-12.),
                        },
                    ])
                    // The search: a bare field in a row of its own.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px(px(18.))
                            .h(px(54.))
                            .text_size(px(15.))
                            .line_height(px(22.))
                            .child(ui::icon(IconName::Search).size(px(18.)).text_color(theme.muted_foreground))
                            .child(div().flex_1().min_w_0().child(self.input.clone()))
                            .child(ui::kbd("esc", &theme)),
                    )
                    .child(div().h_px().bg(theme.border))
                    .child(if rows == 0 {
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_2()
                            .py(px(36.))
                            .child(ui::icon(IconName::Search).size(px(22.)).text_color(theme.muted_foreground.opacity(0.6)))
                            .child(div().text_size(px(13.)).text_color(theme.muted_foreground).child(t("Nothing matches.")))
                            .into_any_element()
                    } else {
                        uniform_list("palette-rows", rows, move |range, _, cx| {
                            let palette = entity.read(cx);
                            let theme = Theme::global(cx);
                            range
                                .map(|position| {
                                    let command = &palette.commands[palette.matches[position]];
                                    let is_selected = position == selected;
                                    let entity = entity.clone();
                                    let (icon_bg, icon_fg) = if is_selected {
                                        (theme.tints.blue.base, theme.tints.blue.foreground)
                                    } else {
                                        (theme.muted, theme.muted_foreground)
                                    };
                                    div()
                                        .id(position)
                                        .h(px(ROW))
                                        .px(px(6.))
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(10.))
                                                .size_full()
                                                .px(px(8.))
                                                .rounded(radius::MD)
                                                .cursor_pointer()
                                                .when(is_selected, |el| el.bg(theme.accent))
                                                .hover(|style| style.bg(theme.accent))
                                                .child(
                                                    div()
                                                        .flex()
                                                        .flex_shrink_0()
                                                        .items_center()
                                                        .justify_center()
                                                        .size(px(24.))
                                                        .rounded(radius::XS)
                                                        .bg(icon_bg)
                                                        .child(ui::icon(command.icon).size_3p5().text_color(icon_fg)),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .text_size(px(13.5))
                                                        .line_height(px(18.))
                                                        .text_color(theme.popover_foreground)
                                                        .font_weight(if is_selected { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                                                        .child(command.label.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .flex_shrink_0()
                                                        .text_size(px(11.5))
                                                        .text_color(theme.muted_foreground)
                                                        .child(t(command.group)),
                                                )
                                                .children(command.keys.map(|keys| ui::kbd(ui::shortcut(keys), theme))),
                                        )
                                        .on_click(move |_, _, cx| entity.update(cx, |palette, cx| palette.run(position, cx)))
                                })
                                .collect()
                        })
                        .track_scroll(self.scroll.clone())
                        .py(px(6.))
                        .h(px(rows.min(VISIBLE) as f32 * ROW + 12.))
                        .into_any_element()
                    })
                    // How to drive it.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            .px(px(18.))
                            .h(px(36.))
                            .border_t_1()
                            .border_color(theme.border)
                            .bg(theme.muted.opacity(0.5))
                            .text_size(px(11.5))
                            .text_color(theme.muted_foreground)
                            .child(div().flex().items_center().gap_1p5().child(ui::kbd("↑↓", &theme)).child(t("move")))
                            .child(div().flex().items_center().gap_1p5().child(ui::kbd("↵", &theme)).child(t("run")))
                            .child(div().flex_1())
                            .child(crate::i18n::tn(rows, "{} command", "{} commands")),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn every_word_has_to_match_and_prefixes_rank_first() {
        assert!(score("New terminal", "term").is_some());
        assert!(score("New terminal", "new term").is_some());
        assert_eq!(score("New terminal", "browser"), None);
        assert!(score("Terminal 2", "ter").unwrap() > score("New terminal", "ter").unwrap());
        assert!(score("Zoom to fit", "fit").unwrap() > score("Outfit", "fit").unwrap());
    }
}
