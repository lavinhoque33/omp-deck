//! Toast stack (bottom-right): info / success / error, auto-dismiss, optional action button.

use std::time::Duration;

use egui::{Align2, Area, Context, Frame, Id, Margin, Order, RichText, vec2};

use crate::icons::{ICON_CIRCLE_ALERT, ICON_CIRCLE_CHECK, ICON_INFO, ICON_X};
use crate::summary::summarize;
use crate::theme::{self, Palette};
use crate::widgets::{icon_text, paint_icon};

const FADE_IN: f32 = 0.18;
const FADE_OUT: f32 = 0.35;
const MAX_VISIBLE: usize = 5;
const WIDTH: f32 = 340.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

impl ToastKind {
    fn lifetime(self, has_action: bool) -> f32 {
        match (self, has_action) {
            (_, true) => 9.0,
            (ToastKind::Error, _) => 7.0,
            _ => 3.5,
        }
    }
}

/// Button shown inside a toast; `id` is returned by [`Toasts::show`] when it is clicked.
#[derive(Clone, Debug)]
pub struct ToastAction {
    pub label: String,
    pub id: u64,
}

struct Item {
    id: u64,
    kind: ToastKind,
    /// First line of the message, bounded in length.
    text: String,
    /// The full message when `text` left something out.
    detail: Option<String>,
    action: Option<ToastAction>,
    born: f64,
}

/// What the user clicked in a toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToastClick {
    Action(u64),
    /// "Copy details": the full message to put on the clipboard.
    Details(String),
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Item>,
    next_id: u64,
}

impl Toasts {
    pub fn info(&mut self, ctx: &Context, text: impl Into<String>) {
        self.push(ctx, ToastKind::Info, text.into(), None);
    }

    pub fn success(&mut self, ctx: &Context, text: impl Into<String>) {
        self.push(ctx, ToastKind::Success, text.into(), None);
    }

    pub fn error(&mut self, ctx: &Context, text: impl Into<String>) {
        self.push(ctx, ToastKind::Error, text.into(), None);
    }

    /// Plain toast of any kind (used for results arriving from worker threads).
    pub fn notify(&mut self, ctx: &Context, kind: ToastKind, text: impl Into<String>) {
        self.push(ctx, kind, text.into(), None);
    }

    pub fn with_action(
        &mut self,
        ctx: &Context,
        kind: ToastKind,
        text: impl Into<String>,
        action: ToastAction,
    ) {
        self.push(ctx, kind, text.into(), Some(action));
    }

    fn push(&mut self, ctx: &Context, kind: ToastKind, text: String, action: Option<ToastAction>) {
        self.next_id += 1;
        let (line, more) = summarize(&text);
        self.items.push(Item {
            id: self.next_id,
            kind,
            detail: more.then_some(text),
            text: line,
            action,
            born: ctx.input(|i| i.time),
        });
        if self.items.len() > MAX_VISIBLE {
            self.items.remove(0);
        }
        ctx.request_repaint();
    }

    /// Draws the stack; returns the id of an action button clicked this frame.
    pub fn show(&mut self, ctx: &Context) -> Option<ToastClick> {
        if self.items.is_empty() {
            return None;
        }
        let pal = Palette::of_ctx(ctx);
        let now = ctx.input(|i| i.time);
        self.items
            .retain(|t| (now - t.born) as f32 <= t.kind.lifetime(t.action.is_some()));
        let mut dismissed = None;
        let mut clicked = None;
        let mut details = None;
        Area::new(Id::unique("omp-deck-toast-stack"))
            .order(Order::Tooltip)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-18.0, -18.0))
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    // Oldest at the top, newest at the bottom corner.
                    for t in &self.items {
                        let age = (now - t.born) as f32;
                        let life = t.kind.lifetime(t.action.is_some());
                        let a = (age / FADE_IN)
                            .min(1.0)
                            .min(((life - age) / FADE_OUT).clamp(0.0, 1.0));
                        let (icon, color) = match t.kind {
                            ToastKind::Info => (
                                ICON_INFO,
                                if theme::neutral() { pal.glyph } else { pal.accent_b },
                            ),
                            ToastKind::Success => (ICON_CIRCLE_CHECK, pal.ok),
                            ToastKind::Error => (ICON_CIRCLE_ALERT, pal.danger),
                        };
                        ui.scope(|ui| {
                            ui.set_opacity(a);
                            Frame::popup(ui.style())
                                .fill(if theme::neutral() {
                                    ui.style().visuals.window_fill
                                } else {
                                    theme::mix(pal.raised, color, 0.06)
                                })
                                .stroke(theme::status_stroke(pal, color, 0.45))
                                .corner_radius(theme::rad(8, 12))
                                .inner_margin(Margin::symmetric(14, 11))
                                .show(ui, |ui| {
                                    ui.set_width(WIDTH);
                                    egui::Sides::new().shrink_left().show(
                                        ui,
                                        |ui| {
                                            ui.spacing_mut().item_spacing.x = 10.0;
                                            ui.label(icon_text(icon, 20.0, color));
                                            ui.vertical(|ui| {
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(&t.text)
                                                            .color(pal.text_strong),
                                                    )
                                                    .wrap_mode(egui::TextWrapMode::Wrap),
                                                );
                                                if let Some(full) = &t.detail
                                                    && ui
                                                        .add(
                                                            egui::Button::new(
                                                                RichText::new("Copy details")
                                                                    .small()
                                                                    .color(pal.weak),
                                                            )
                                                            .frame(false),
                                                        )
                                                        .on_hover_text(
                                                            "Copy the full message to the clipboard",
                                                        )
                                                        .clicked()
                                                {
                                                    details = Some((t.id, full.clone()));
                                                }
                                            });
                                        },
                                        |ui| {
                                            let (r, resp) = ui.allocate_exact_size(
                                                vec2(18.0, 18.0),
                                                egui::Sense::click(),
                                            );
                                            let col = if resp.hovered() {
                                                pal.text_strong
                                            } else {
                                                pal.weak
                                            };
                                            paint_icon(
                                                ui.painter(),
                                                r.center(),
                                                Align2::CENTER_CENTER,
                                                ICON_X,
                                                15.0,
                                                col,
                                            );
                                            if resp.clicked() {
                                                dismissed = Some(t.id);
                                            }
                                            if let Some(act) = &t.action
                                                && ui
                                                    .add(
                                                        egui::Button::new(
                                                            theme::strong(&act.label)
                                                                .color(pal.text_strong),
                                                        )
                                                        .fill(if theme::neutral() {
                                                            pal.surface
                                                        } else {
                                                            color.gamma_multiply(0.25)
                                                        })
                                                        .stroke(theme::status_stroke(
                                                            pal, color, 0.7,
                                                        ))
                                                        .corner_radius(8),
                                                    )
                                                    .clicked()
                                            {
                                                clicked = Some(ToastClick::Action(act.id));
                                                dismissed = Some(t.id);
                                            }
                                        },
                                    );
                                });
                        });
                    }
                });
            });
        let dismissed = dismissed.or(details.as_ref().map(|(id, _)| *id));
        if let Some(id) = dismissed {
            self.items.retain(|t| t.id != id);
        }
        ctx.request_repaint_after(Duration::from_millis(16));
        clicked.or(details.map(|(_, text)| ToastClick::Details(text)))
    }
}
