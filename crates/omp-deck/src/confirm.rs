//! Modal dialogs: the shared chrome ([`show_dialog`]) and the yes/no confirmation built on it.
//! Both are gradient-rule cards ([`crate::card`]) with Drafted buttons.

use egui::{Color32, Context, Id, Key, RichText, Ui};

use crate::card;
use crate::icons::{ICON_INFO, ICON_TRIANGLE_ALERT, Icon};
use crate::theme::Palette;
use crate::widgets::{DraftButton, DraftKind};

/// What to ask. `danger` paints the confirm button red and gives the header an alert tile.
pub struct Confirm {
    pub title: String,
    pub body: String,
    pub confirm_label: String,
    pub danger: bool,
}

/// Chrome of a form dialog: header, body supplied by the caller, Cancel / confirm footer.
#[derive(Default)]
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
    /// Header glyph; `None` = an alert for danger / toned dialogs, else info. A danger dialog's
    /// confirm button carries the same glyph.
    pub icon: Option<Icon>,
    /// Weak line under the title.
    pub subtitle: Option<&'a str>,
    /// Header tone (icon tile, wash, rule), e.g. `pal.warn`; `None` = `pal.danger` when
    /// `danger`, else the accent pair.
    pub tone: Option<Color32>,
}

impl Dialog<'_> {
    fn tone(&self, pal: &Palette) -> Color32 {
        self.tone.unwrap_or(if self.danger {
            pal.danger
        } else {
            Color32::TRANSPARENT
        })
    }

    fn icon(&self) -> Icon {
        self.icon.unwrap_or(if self.danger || self.tone.is_some() {
            ICON_TRIANGLE_ALERT
        } else {
            ICON_INFO
        })
    }
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
    let icon = d.icon();
    let mut head = card::Header::new(icon, d.title).tone(d.tone(pal));
    if let Some(s) = d.subtitle {
        head = head.subtitle(s);
    }
    let hints: &[(&str, &str)] = if d.enter_confirms && d.confirm_enabled {
        &[("Enter", "confirms")]
    } else {
        &[("Esc", "cancels")]
    };
    let mut result = None;
    let dismissed = card::modal(
        ctx,
        Id::unique(("omp-deck-dialog", d.id)),
        d.width,
        &head,
        |ui| {
            body(ui);
            card::footer(ui, hints, |ui| {
                let kind = if d.danger {
                    DraftKind::Danger
                } else {
                    DraftKind::Primary
                };
                let mut confirm = DraftButton::new(d.confirm_label)
                    .kind(kind)
                    .enabled(d.confirm_enabled)
                    .min_width(96.0);
                if d.danger {
                    confirm = confirm.icon(icon);
                }
                let confirm = confirm.show(ui);
                if confirm.clicked() {
                    result = Some(true);
                }
                if DraftButton::new(cancel_label)
                    .min_width(84.0)
                    .show(ui)
                    .clicked()
                {
                    result = Some(false);
                }
            });
        },
    );
    if result.is_none()
        && d.enter_confirms
        && d.confirm_enabled
        && ctx.input(|i| i.key_pressed(Key::Enter))
    {
        result = Some(true);
    }
    if result.is_none() && dismissed {
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
            width: 420.0,
            confirm_label: &confirm.confirm_label,
            confirm_enabled: true,
            danger: confirm.danger,
            ..Dialog::default()
        },
        |ui| {
            ui.add(
                egui::Label::new(RichText::new(&confirm.body).color(pal.text))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        },
    )
}
