//! Pieces shared by the hover cards of tabs, sessions, the title strip and the status bar (all
//! laid out by [`crate::card::hover`]): the live-state badge of a header's right slot, its tone,
//! and the value styles of the key/value rows (ids as teal badges, paths in monospace, plain
//! facts in strong text).

use std::path::Path;

use deck_service::status::TabStatus;
use egui::{Color32, Label, RichText, Ui};

use crate::badge::{Badge, Hue};
use crate::theme::Palette;

/// Text size of the values in a hover card's rows (badges and plain text alike).
pub const VALUE: f32 = 11.5;

/// Header right slot: the live state of a tab or session; an unclean exit shows its code in red.
pub fn status_badge(ui: &mut Ui, status: TabStatus, exit_code: Option<i32>) {
    let pal = Palette::of_ui(ui);
    let exit;
    let (text, color) = match status {
        TabStatus::Busy => ("working", Hue::Blue.color(pal)),
        TabStatus::Attention => ("needs input", Hue::Amber.color(pal)),
        TabStatus::Bell => ("bell", Hue::Amber.color(pal)),
        TabStatus::Idle => ("idle", Hue::Green.color(pal)),
        TabStatus::Exited { clean: true } => ("exited", pal.weak),
        TabStatus::Exited { clean: false } => match exit_code {
            Some(code) => {
                exit = format!("exit {code}");
                (exit.as_str(), pal.danger)
            }
            None => ("exited", pal.danger),
        },
    };
    Badge::single(ui.painter(), None, text, color, VALUE).show(ui);
}

/// Header tone of a status: amber while omp waits for the user, red after a failed exit, else
/// the accent pair.
pub fn status_tone(pal: &Palette, status: TabStatus) -> Color32 {
    match status {
        TabStatus::Attention | TabStatus::Bell => pal.warn,
        TabStatus::Exited { clean: false } => pal.danger,
        _ => Color32::TRANSPARENT,
    }
}

/// A path value (`~`-abbreviated): monospace, strong, wrapped to the card's width.
pub fn path(ui: &mut Ui, path: &Path) {
    mono(ui, &deck_core::sessions::abbreviate_home(path));
}

/// Monospace strong value, wrapped to the card's width (paths, URLs, backend names).
pub fn mono(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add(
        Label::new(
            RichText::new(text)
                .monospace()
                .size(VALUE)
                .color(pal.text_strong),
        )
        .wrap(),
    );
}

/// Plain strong value, wrapped to the card's width.
pub fn strong(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add(Label::new(RichText::new(text).size(VALUE + 1.0).color(pal.text_strong)).wrap());
}

/// Weak note line (explanations under the rows).
pub fn note(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add(Label::new(RichText::new(text).size(VALUE).color(pal.weak)).wrap());
}

/// An id as a teal badge. Long ids show their last 8 characters (omp's UUIDv7 ids share their
/// leading timestamp digits, the tail tells them apart).
pub fn id_badge(ui: &mut Ui, id: &str) {
    let tail = id.char_indices().rev().nth(7).map(|(i, _)| &id[i..]);
    match tail {
        Some(t) if t.len() < id.len() => {
            crate::badge::show(ui, None, &format!("…{t}"), Hue::Teal, VALUE);
        }
        _ => {
            crate::badge::show(ui, None, id, Hue::Teal, VALUE);
        }
    }
}
