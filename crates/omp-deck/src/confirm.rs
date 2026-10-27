//! Modal dialogs: the shared chrome ([`show_dialog`]) and the yes/no confirmation built on it.
//! Both are gradient-rule cards ([`crate::card`]) with Drafted buttons.

use egui::{Color32, Context, Id, Key, Modifiers, RichText, Ui};

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
    /// Pressing Ctrl+Enter confirms (dialogs with a multiline editor, where Enter adds a line).
    pub ctrl_enter_confirms: bool,
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
    let hints: &[(&str, &str)] = if !d.confirm_enabled {
        &[("Esc", "cancels")]
    } else if d.enter_confirms {
        &[("Enter", "confirms")]
    } else if d.ctrl_enter_confirms {
        &[("Ctrl+Enter", "confirms")]
    } else {
        &[("Esc", "cancels")]
    };
    // Taken before the body runs, so the dialog's text editor never sees the chord.
    let ctrl_enter = d.ctrl_enter_confirms
        && d.confirm_enabled
        && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
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
    if result.is_none() && ctrl_enter {
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

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, RawInput, TextEdit};

    fn enter(modifiers: Modifiers) -> Event {
        Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// Shows a dialog with a focused multiline editor for a few quiet frames, then one frame
    /// carrying `event`; returns that frame's answer and the editor text.
    fn press(d: &Dialog<'_>, event: Event) -> (Option<bool>, String) {
        let ctx = Context::default();
        crate::theme::apply(
            &ctx,
            deck_core::config::UiStyle::Neutral,
            deck_core::config::Density::Compact,
            deck_core::config::UiFont::Auto,
            true,
        );
        let mut text = String::from("focus");
        let mut answer = None;
        for frame in 0..4 {
            let mut input = RawInput::default();
            if frame == 3 {
                input.events.push(event.clone());
            }
            let mut out = ctx.run_ui(input, |ui| {
                let c = ui.ctx().clone();
                answer = show_dialog(&c, d, |ui| {
                    ui.add(TextEdit::multiline(&mut text)).request_focus();
                });
            });
            out.textures_delta.clear();
        }
        (answer, text)
    }

    fn handoff_like(confirm_enabled: bool) -> Dialog<'static> {
        Dialog {
            id: "t",
            title: "T",
            width: 400.0,
            confirm_label: "Go",
            confirm_enabled,
            ctrl_enter_confirms: true,
            ..Dialog::default()
        }
    }

    #[test]
    fn ctrl_enter_confirms_without_reaching_the_editor() {
        let (answer, text) = press(&handoff_like(true), enter(Modifiers::COMMAND));
        assert_eq!(answer, Some(true));
        assert_eq!(text, "focus");
    }

    #[test]
    fn plain_enter_types_a_newline_instead_of_confirming() {
        let (answer, text) = press(&handoff_like(true), enter(Modifiers::NONE));
        assert_eq!(answer, None);
        assert_eq!(text, "focus\n");
    }

    #[test]
    fn ctrl_enter_does_nothing_while_confirm_is_disabled_or_not_opted_in() {
        assert_eq!(
            press(&handoff_like(false), enter(Modifiers::COMMAND)).0,
            None
        );
        let plain = Dialog {
            ctrl_enter_confirms: false,
            ..handoff_like(true)
        };
        assert_eq!(press(&plain, enter(Modifiers::COMMAND)).0, None);
    }
}
