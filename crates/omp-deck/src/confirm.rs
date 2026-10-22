//! Modal dialogs: the shared chrome ([`show_dialog`]) and the yes/no confirmation built on it.

use egui::{
    Button, Color32, Context, Frame, Id, Key, Margin, Modal, RichText, Shadow, Sides, Ui, vec2,
};

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
    let pal = Palette::of_ctx(ctx);
    let mut result = None;
    let modal = Modal::new(Id::unique(("omp-deck-dialog", d.id)))
        .backdrop_color(Color32::from_black_alpha(150))
        .frame(
            Frame::popup(&ctx.global_style())
                .fill(pal.raised)
                .corner_radius(16)
                .inner_margin(Margin::same(22))
                .shadow(Shadow {
                    offset: [0, 20],
                    blur: 48,
                    spread: 0,
                    color: Color32::from_black_alpha(180),
                }),
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
                        (pal.danger.gamma_multiply(0.85), Color32::WHITE)
                    } else {
                        (pal.accent, Color32::WHITE)
                    };
                    let confirm = ui.add_enabled(
                        d.confirm_enabled,
                        Button::new(RichText::new(d.confirm_label).color(fg))
                            .fill(fill)
                            .min_size(vec2(96.0, 32.0))
                            .corner_radius(10),
                    );
                    if confirm.clicked() {
                        result = Some(true);
                    }
                    if ui
                        .add(
                            Button::new("Cancel")
                                .min_size(vec2(84.0, 32.0))
                                .corner_radius(10),
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
