//! Settings → OmpDeck → Notifications: the toggles and the test button.

use deck_core::config::NotifyConfig;
use egui::{Button, RichText, Ui, vec2};

use crate::icons::ICON_BELL;
use crate::theme::{self, Palette};
use crate::widgets::{card, icon_text, setting_row, toggle};

#[derive(Debug, PartialEq, Eq)]
pub enum NotifyAction {
    Desktop(bool),
    TurnFinished(bool),
    Attention(bool),
    Exited(bool),
    OnlyWhenUnfocused(bool),
    /// Show a notification now.
    Test,
}

/// What the card shows besides the stored toggles.
pub struct NotifyInput<'a> {
    pub cfg: &'a NotifyConfig,
    /// Why the platform cannot show notifications (reported by the notifier thread).
    pub unavailable: Option<&'a str>,
}

pub fn card_ui(ui: &mut Ui, input: &NotifyInput<'_>) -> Vec<NotifyAction> {
    let pal = Palette::of_ui(ui);
    let cfg = input.cfg;
    let mut actions = Vec::new();
    card(ui, "Notifications", |ui| {
        ui.label(
            RichText::new(
                "omp's own notifications are turned off inside OmpDeck; these settings decide everything.",
            )
            .small()
            .color(pal.weak),
        );
        ui.add_space(10.0);
        let mut row =
            |ui: &mut Ui, title: &str, sub: &str, value: bool, make: fn(bool) -> NotifyAction| {
                setting_row(ui, title, sub, |ui| {
                    let mut on = value;
                    if toggle(ui, &mut on).changed() {
                        actions.push(make(on));
                    }
                });
            };
        ui.label(theme::section_caption("Notify me about", pal.weak));
        ui.add_space(4.0);
        row(
            ui,
            "A turn finished",
            "…or stopped with an error (after working for at least 3 seconds)",
            cfg.turn_finished,
            NotifyAction::TurnFinished,
        );
        ui.add_space(4.0);
        row(
            ui,
            "Needs attention",
            "omp asks a question or waits for an approval",
            cfg.attention,
            NotifyAction::Attention,
        );
        ui.add_space(4.0);
        row(
            ui,
            "Session ended",
            "The omp process of a tab exited",
            cfg.exited,
            NotifyAction::Exited,
        );
        ui.add_space(10.0);
        ui.label(theme::section_caption("Desktop pop-ups", pal.weak));
        ui.add_space(4.0);
        row(
            ui,
            "Show desktop pop-ups",
            "Besides the bell in the title bar, tell me even while OmpDeck is in the background",
            cfg.desktop,
            NotifyAction::Desktop,
        );
        ui.add_enabled_ui(cfg.desktop, |ui| {
            ui.add_space(4.0);
            row(
                ui,
                "Only when OmpDeck is not focused",
                "Off: background tabs also pop up while the window has focus",
                cfg.only_when_unfocused,
                NotifyAction::OnlyWhenUnfocused,
            );
        });
        ui.add_space(4.0);
        let sub = match input.unavailable {
            Some(why) => format!("Notifications are unavailable: {why}"),
            None => "Clicking the notification brings the tab to the front".to_owned(),
        };
        setting_row(ui, "Test notification", &sub, |ui| {
            let button = Button::new((
                icon_text(ICON_BELL, 16.0, pal.glyph),
                RichText::new("Send test notification"),
            ))
            .min_size(vec2(0.0, 30.0));
            if ui.add(button).clicked() {
                actions.push(NotifyAction::Test);
            }
        });
    });
    actions
}
