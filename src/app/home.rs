//! The Home page.

use super::*;

impl AppShell {
    /// Home page: the hero, laid out like the one on ui.astralyx.dev.
    pub(super) fn render_home(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let toggle_label = match theme.mode {
            Mode::Dark => "Switch to light",
            Mode::Light => "Switch to dark",
        };

        div()
            .flex_1()
            .flex()
            .flex_col()
            .justify_center()
            .items_start()
            .px(px(48.))
            .child(
                // text-[11px] font-medium uppercase text-muted-foreground
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child("RUST · GPUI · ASTRALYX UI"),
            )
            .child(
                // text-5xl leading-[0.95] font-semibold
                div()
                    .mt(px(20.))
                    .text_size(px(48.))
                    .line_height(relative(0.95))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Hello, World!"),
            )
            .child(
                // text-base leading-relaxed text-muted-foreground
                div()
                    .mt(px(20.))
                    .max_w(px(440.))
                    .text_size(px(16.))
                    .line_height(relative(1.625))
                    .text_color(theme.muted_foreground)
                    .child(
                        "A native GPUI window wearing the Astralyx UI design \
                         tokens: colors, radii and type from ui.astralyx.dev.",
                    ),
            )
            .child(
                div()
                    .mt(px(32.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("toggle-theme", toggle_label)
                            .size(ButtonSize::Large)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let current = Theme::global(cx).mode;
                                this.mode_override = Some(current.toggled());
                                this.apply_theme(window, cx);
                            })),
                    )
                    .when(self.mode_override.is_some(), |row| {
                        row.child(
                            Button::new("follow-system", "Use system")
                                .variant(Variant::Ghost)
                                .size(ButtonSize::Large)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.mode_override = None;
                                    this.apply_theme(window, cx);
                                })),
                        )
                    }),
            )
            .child(
                // Command chip: border bg-card rounded-lg h-11 px-4 font-mono text-sm
                div()
                    .mt(px(24.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .h_11()
                    .px_4()
                    .rounded(radius::LG)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.card)
                    .font_family(FONT_MONO)
                    .text_size(px(14.))
                    .child(
                        div()
                            .text_color(theme.muted_foreground.opacity(0.6))
                            .child("$"),
                    )
                    .child("cargo run"),
            )
            .into_any_element()
    }
}
