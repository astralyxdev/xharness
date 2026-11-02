//! A modal that asks to confirm something consequential, styled as the
//! Astralyx `AlertDialog`.
//!
//! Unlike the regular dialog, a click on the backdrop does nothing: a
//! destructive confirmation needs a deliberate answer. Escape still cancels.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, SharedString, Window,
    prelude::*, px,
};

use crate::i18n::t;
use crate::ui::actions::Cancel;
use crate::ui::theme::Theme;
use crate::ui::{self, Button, Variant};

const KEY_CONTEXT: &str = "AlertDialog";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Cancel, Some(KEY_CONTEXT))]);
}

pub enum AlertDialogEvent {
    Confirm,
    Cancel,
}

pub struct AlertDialog {
    title: SharedString,
    description: SharedString,
    confirm_label: SharedString,
    focus_handle: FocusHandle,
}

impl EventEmitter<AlertDialogEvent> for AlertDialog {}

impl Focusable for AlertDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AlertDialog {
    /// A destructive confirmation (the confirm button is `--destructive`).
    /// Takes focus so Escape works straight away.
    pub fn destructive(
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        Self {
            title: title.into(),
            description: description.into(),
            confirm_label: confirm_label.into(),
            focus_handle,
        }
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(AlertDialogEvent::Cancel);
    }
}

impl Render for AlertDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx);

        ui::modal_backdrop().child(
            ui::dialog_surface("alert-dialog", theme)
                .key_context(KEY_CONTEXT)
                .track_focus(&self.focus_handle)
                .on_action(cx.listener(Self::cancel))
                // alert-dialog-header: gap-2 p-6
                .child(ui::dialog_header(
                    self.title.clone(),
                    self.description.clone(),
                    px(8.),
                    theme,
                ))
                .child(
                    ui::dialog_footer(theme)
                        .child(
                            Button::new("alert-cancel", t("Cancel"))
                                .variant(Variant::Soft)
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(AlertDialogEvent::Cancel)
                                })),
                        )
                        .child(
                            Button::new("alert-confirm", self.confirm_label.clone())
                                .variant(Variant::Destructive)
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(AlertDialogEvent::Confirm)
                                })),
                        ),
                ),
        )
    }
}
