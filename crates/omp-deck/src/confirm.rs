//! Modal dialogs: the shared chrome ([`show_dialog`]) and the yes/no confirmation built on it.

use egui::{Button, Context, Frame, Id, Key, Modal, RichText, Sides, Ui, vec2};

use crate::theme::{self, Palette};

/// What to ask. `danger` paints the confirm button red.
pub struct Confirm {
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    pub danger: bool,
}

/// Chrome of a form dialog: title, body supplied by the caller, Cancel / confirm footer.
pub struct Dialog<'a> {
    /// Stable per dialog kind (text fields inside need a stable widget id).
    pub id: &'a str,
    pub title: &'a str,
    pub width: f32,
    pub confirm_label: &'a str,
    pub confirm_enabled: bool,
    pub danger: bool,
    /// Pressing Enter confirms (leave off when the body has a multiline editor).
    pub enter_confirms: bool,
}

/// Shows the modal; `Some(true)` = confirmed, `Some(false)` = cancelled / dismissed.
pub fn show_dialog(ctx: &Context, d: &Dialog<'_>, body: impl FnOnce(&mut Ui)) -> Option<bool> {
    show_dialog_labeled(ctx, d, "Cancel", body)
}

/// [`show_dialog`] with a custom dismiss-button label (e.g. "Close" when nothing can be confirmed).
pub fn show_dialog_labeled(
    ctx: &Context,
    d: &Dialog<'_>,
    cancel_label: &str,
    body: impl FnOnce(&mut Ui),
) -> Option<bool> {
    let pal = Palette::of_ctx(ctx);
    let mut result = None;
    let modal = Modal::new(Id::unique(("omp-deck-dialog", d.id)))
        .backdrop_color(theme::backdrop(ctx.global_style().visuals.dark_mode))
        .frame(
            Frame::popup(&ctx.global_style())
                .fill(if theme::flat() {
                    ctx.global_style().visuals.window_fill
                } else {
                    pal.raised
                })
                .stroke(egui::Stroke::new(1.0, pal.stroke))
                .corner_radius(theme::popup_radius())
                .inner_margin(theme::dialog_margin())
                .shadow(theme::modal_shadow(ctx.global_style().visuals.dark_mode)),
        )
        .show(ctx, |ui| {
            ui.set_width(d.width);
            ui.label(theme::strong(d.title).size(18.0).color(pal.text_strong));
            ui.add_space(8.0);
            body(ui);
            ui.add_space(20.0);
            Sides::new().show(
                ui,
                |_| {},
                |ui| {
                    let (fill, fg) = if d.danger {
                        (
                            pal.danger
                                .gamma_multiply(if theme::flat() { 1.0 } else { 0.85 }),
                            if theme::flat() {
                                theme::ink_on(pal.danger)
                            } else {
                                egui::Color32::WHITE
                            },
                        )
                    } else {
                        (pal.accent, theme::on_accent(pal))
                    };
                    let confirm = ui.add_enabled(
                        d.confirm_enabled,
                        Button::new(RichText::new(d.confirm_label).color(fg))
                            .fill(fill)
                            .min_size(vec2(96.0, 32.0))
                            .corner_radius(theme::rad(8, 10)),
                    );
                    if confirm.clicked() {
                        result = Some(true);
                    }
                    if ui
                        .add(
                            Button::new(cancel_label)
                                .min_size(vec2(84.0, 32.0))
                                .corner_radius(theme::rad(8, 10)),
                        )
                        .clicked()
                    {
                        result = Some(false);
                    }
                },
            );
        });
    if result.is_none()
        && d.enter_confirms
        && d.confirm_enabled
        && ctx.input(|i| i.key_pressed(Key::Enter))
    {
        result = Some(true);
    }
    if result.is_none() && modal.should_close() {
        result = Some(false);
    }
    result
}

/// Shows the confirmation; `Some(true)` = confirmed, `Some(false)` = cancelled / dismissed.
pub fn show(ctx: &Context, id: &str, confirm: &Confirm) -> Option<bool> {
    let pal = Palette::of_ctx(ctx);
    show_dialog(
        ctx,
        &Dialog {
            id,
            title: &confirm.title,
            width: 400.0,
            confirm_label: &confirm.confirm_label,
            confirm_enabled: true,
            danger: confirm.danger,
            enter_confirms: false,
        },
        |ui| {
            ui.add(
                egui::Label::new(RichText::new(&confirm.body).color(pal.weak))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        },
    )
}
