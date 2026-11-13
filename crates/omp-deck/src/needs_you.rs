//! The title bar's **Needs you** inbox: every dialog an omp session waits on
//! (`ControllerInfo.pending_ui` over all controllers), answered in place with native controls.
//! The button shows only while something is pending (or the popover is open); answering never
//! focuses or switches tabs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use deck_core::ctl::{
    PendingUi, PendingUiKind, UiFormBlock, UiFormField, UiFormSection, UiQuestion, UiSpan,
};
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::ControllerInfo;
use deck_service::ui_answer::UiAnswer;
use egui::text::LayoutJob;
use egui::{
    Align, Align2, Color32, CursorIcon, FontFamily, FontId, Frame, Id, Layout, Margin, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, TextFormat, Ui, vec2,
};

use crate::badge::{self, Hue};
use crate::icons::{
    ICON_BRAIN, ICON_CIRCLE, ICON_CIRCLE_CHECK, ICON_CIRCLE_DOT, ICON_CPU, ICON_FOLDER,
    ICON_HOURGLASS, ICON_MESSAGE_CIRCLE_QUESTION, ICON_SQUARE,
};
use crate::model_picker::ModelsCache;
use crate::needs_you_model::{self as model, Draft, FormDraft, Tone};
use crate::sidebar::format_age;
use crate::theme::{self, Palette};
use crate::widgets::{DraftButton, DraftKind, icon_text, paint_icon};

const BUTTON_ID: &str = "needs-you";
const WIDTH: f32 = 420.0;
const MAX_HEIGHT_SHARE: f32 = 0.7;
const PAD: f32 = 10.0;
/// Controls of an answered request stay disabled this long (seconds) while the gateway relays
/// the answer and the request leaves the list.
const SENT_HOLD: f64 = 6.0;

type Key = (SessionTarget, u64);

/// The pending dialogs of all controllers, rebuilt only when the gateway replaces its controller
/// list (newest session first, each session's dialogs oldest first), with the time each was
/// first seen (its age and timeout countdown).
#[derive(Default)]
pub struct Index {
    src: Option<Arc<Vec<ControllerInfo>>>,
    /// (controller, dialog) positions into `src`, with when the dialog was first seen.
    order: Vec<(usize, usize, SystemTime)>,
    seen: HashMap<Key, SystemTime>,
}

impl Index {
    pub fn refresh(&mut self, controllers: Option<Arc<Vec<ControllerInfo>>>, now: SystemTime) {
        let same = match (&self.src, &controllers) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.order.clear();
        if let Some(list) = &controllers {
            self.seen.retain(|(target, id), _| {
                list.iter()
                    .any(|i| &i.target == target && i.pending_ui.iter().any(|u| u.id == *id))
            });
            for (c, info) in list.iter().enumerate() {
                for (u, ui) in info.pending_ui.iter().enumerate() {
                    let seen = *self.seen.entry((info.target.clone(), ui.id)).or_insert(now);
                    self.order.push((c, u, seen));
                }
            }
            self.order.sort_by(|a, b| {
                list[b.0]
                    .spawned_unix_ms
                    .cmp(&list[a.0].spawned_unix_ms)
                    .then(a.0.cmp(&b.0))
                    .then(a.1.cmp(&b.1))
            });
        } else {
            self.seen.clear();
        }
        self.src = controllers;
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    fn entries(&self) -> impl Iterator<Item = Entry<'_>> {
        let list = self.src.as_deref();
        self.order.iter().filter_map(move |&(c, u, seen)| {
            let info = list?.get(c)?;
            let ui = info.pending_ui.get(u)?;
            Some(Entry { info, ui, seen })
        })
    }
}

struct Entry<'a> {
    info: &'a ControllerInfo,
    ui: &'a PendingUi,
    seen: SystemTime,
}

pub struct Input<'a> {
    pub index: &'a Index,
    pub models: &'a ModelsCache,
    pub now: SystemTime,
}

/// What the popover asks the app to do.
#[derive(Debug)]
pub enum Action {
    Answer {
        target: SessionTarget,
        request: u64,
        answer: UiAnswer,
    },
    /// Show the session (the only action that switches tabs).
    Open(SessionTarget),
}

/// The popover's id in egui's popup memory (Ctrl+Shift+I and the palette open it).
pub fn popup_id() -> Id {
    Id::unique(BUTTON_ID).with("popup")
}

/// Width of [`button`], or 0 while it is hidden.
pub fn width(input: &Input<'_>, ctx: &egui::Context) -> f32 {
    if shown(input, ctx) {
        crate::notify_panel::bell_width()
    } else {
        0.0
    }
}

fn shown(input: &Input<'_>, ctx: &egui::Context) -> bool {
    input.index.len() > 0 || egui::Popup::is_id_open(ctx, popup_id())
}

/// The title bar button (while something is pending or the popover is open) and its popover.
pub fn button(ui: &mut Ui, input: &Input<'_>) -> Option<Action> {
    if !shown(input, ui.ctx()) {
        return None;
    }
    let pal = Palette::of_ui(ui);
    let side = crate::notify_panel::bell_width();
    let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    let resp = ui.interact(rect, Id::unique(BUTTON_ID), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), popup_id());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() || open, 0.1);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let (radius, size) = if theme::flat() { (6, 16.0) } else { (9, 19.0) };
        p.rect_filled(
            rect,
            radius,
            theme::hover_fill(pal).gamma_multiply(0.85 * hov),
        );
        let n = input.index.len();
        paint_icon(
            p,
            rect.center(),
            Align2::CENTER_CENTER,
            ICON_MESSAGE_CIRCLE_QUESTION,
            size,
            if n > 0 {
                theme::mix(pal.warn, pal.text_strong, hov * 0.4)
            } else {
                theme::mix(pal.weak, pal.text_strong, hov)
            },
        );
        if let Some(text) = model::badge_text(n) {
            paint_badge(ui, rect, &text, pal);
        }
    }
    let resp = resp
        .on_hover_text("Needs you (Ctrl+Shift+I)")
        .on_hover_cursor(CursorIcon::PointingHand);
    theme::dropdown_menu(&resp, WIDTH, |ui| popover(ui, input)).and_then(|r| r.inner)
}

/// Warn-coloured count pill on the button's top-right corner.
fn paint_badge(ui: &Ui, button: Rect, text: &str, pal: &Palette) {
    let fill = pal.warn;
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::font(10.0), Color32::BLACK);
    let size = vec2((galley.size().x + 8.0).max(15.0), 15.0);
    let rect = Rect::from_min_size(
        egui::pos2(button.right() - size.x + 3.0, button.top() - 1.0),
        size,
    );
    let p = ui.painter();
    p.rect(
        rect,
        8,
        fill,
        Stroke::new(1.5, pal.bar),
        StrokeKind::Outside,
    );
    p.galley(rect.center() - galley.size() / 2.0, galley, Color32::BLACK);
}

/// Per-request editor state kept in egui's temporary memory between frames.
#[derive(Clone, Default)]
struct Memory {
    drafts: HashMap<Key, Draft>,
    /// When an answer was sent (egui time): its controls stay disabled for [`SENT_HOLD`].
    sent: HashMap<Key, f64>,
}

fn popover(ui: &mut Ui, input: &Input<'_>) -> Option<Action> {
    let pal = Palette::of_ui(ui);
    let mut action = None;
    ui.set_width(WIDTH);
    ui.spacing_mut().item_spacing.y = 6.0;
    let count = input.index.len();
    ui.horizontal(|ui| {
        ui.add_space(PAD - 2.0);
        ui.spacing_mut().item_spacing.x = 10.0;
        crate::card::icon_tile(ui, 30.0, ICON_MESSAGE_CIRCLE_QUESTION, pal.warn, true);
        ui.label(theme::strong("Needs you").size(14.5).color(pal.text_strong));
        if count > 0 {
            badge::show(ui, None, &count.to_string(), Hue::Amber, 11.0);
        }
    });
    let left = ui.min_rect().left();
    crate::card::header_rule(ui, left, left + WIDTH, ui.cursor().top(), pal.warn);
    if count == 0 {
        ui.add_space(14.0);
        ui.vertical_centered(|ui| {
            ui.label(icon_text(ICON_CIRCLE_CHECK, 26.0, pal.ok));
            ui.add_space(2.0);
            ui.label(RichText::new("Nothing is waiting for you").color(pal.weak));
        });
        ui.add_space(16.0);
        return None;
    }
    let mem_id = popup_id().with("memory");
    let mut mem: Memory = ui
        .ctx()
        .data_mut(|d| d.remove_temp(mem_id))
        .unwrap_or_default();
    let now_t = ui.ctx().input(|i| i.time);
    mem.drafts.retain(|k, _| {
        input
            .index
            .entries()
            .any(|e| e.info.target == k.0 && e.ui.id == k.1)
    });
    mem.sent.retain(|_, at| now_t - *at < SENT_HOLD);
    let max_h = ui.ctx().content_rect().height() * MAX_HEIGHT_SHARE;
    // The popup's area keeps last frame's size as this frame's room, so a scroll area left to
    // the room it is given never grows past the first (sizing-pass) height: ask for the full
    // share whenever the cards overflow it.
    ScrollArea::vertical()
        .max_height(max_h)
        .min_scrolled_height(max_h)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            for e in input.index.entries() {
                let key = (e.info.target.clone(), e.ui.id);
                let sending = mem.sent.contains_key(&key);
                let draft = mem
                    .drafts
                    .entry(key.clone())
                    .or_insert_with(|| model::initial(e.ui));
                // Ids of the card and every control in it follow the request, never its
                // position: a shifting list must not move a click onto another request.
                let picked = ui
                    .push_id((&key.0, key.1), |ui| {
                        request_card(ui, input, &e, draft, sending)
                    })
                    .inner;
                if let Some(a) = picked {
                    if let Action::Answer { .. } = a {
                        mem.sent.insert(key, now_t);
                    }
                    action = Some(a);
                }
            }
        });
    if !mem.sent.is_empty() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
    ui.ctx().data_mut(|d| d.insert_temp(mem_id, mem));
    if matches!(action, Some(Action::Open(_))) {
        ui.close();
    }
    action
}

/// One request: who asks (title + model/thinking, project), the kind and age, then its
/// controls; Cancel and "Open session" at the bottom.
fn request_card(
    ui: &mut Ui,
    input: &Input<'_>,
    e: &Entry<'_>,
    draft: &mut Draft,
    sending: bool,
) -> Option<Action> {
    let pal = Palette::of_ui(ui);
    let mut action = None;
    let target = &e.info.target;
    let request = e.ui.id;
    let answer = |answer: UiAnswer| Action::Answer {
        target: target.clone(),
        request,
        answer,
    };
    Frame::NONE
        .fill(pal.surface)
        .stroke(Stroke::new(1.0, pal.stroke))
        .corner_radius(theme::ui_rad(8))
        .inner_margin(Margin::same(PAD as i8))
        .outer_margin(Margin::symmetric(PAD as i8 - 4, 0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            // Who asks: session title, model and thinking.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(
                    theme::strong(e.info.label.as_str())
                        .size(13.0)
                        .color(pal.text_strong),
                );
                if let Some(sel) = e.info.current_model.as_deref() {
                    let name = input.models.get(sel).map_or_else(
                        || crate::agent_model::short_model(sel),
                        |m| deck_service::statusfmt::short_model_name(&m.name).to_owned(),
                    );
                    badge::show(ui, Some(ICON_CPU), &name, Hue::Violet, 11.0);
                }
                if let Some(t) = e.info.current_thinking.as_deref().filter(|t| *t != "off") {
                    badge::show(ui, Some(ICON_BRAIN), t, Hue::Rose, 11.0);
                }
            });
            // Where, what kind, how long ago, when omp answers by itself.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(icon_text(ICON_FOLDER, 11.0, pal.weak));
                ui.label(
                    RichText::new(deck_core::sessions::abbreviate_home(&e.info.cwd))
                        .font(theme::label_font(11.0))
                        .color(pal.weak),
                );
                badge::show(ui, None, model::kind_label(e.ui.kind), Hue::Amber, 10.5);
                let age = format_age(input.now, e.seen);
                ui.label(
                    RichText::new(if age == "now" {
                        "just now".to_owned()
                    } else {
                        format!("{age} ago")
                    })
                    .font(theme::label_font(10.5))
                    .color(pal.weak),
                );
                if let Some(ms) = e.ui.timeout_ms {
                    let waited = input
                        .now
                        .duration_since(e.seen)
                        .map_or(0, |d| d.as_millis() as u64);
                    let left = ms.saturating_sub(waited) / 1000;
                    ui.label(icon_text(ICON_HOURGLASS, 11.0, pal.warn));
                    ui.label(
                        RichText::new(format!("{left}s"))
                            .font(theme::label_font(10.5))
                            .color(pal.warn),
                    );
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs(1));
                }
            });
            ui.add(
                egui::Label::new(
                    RichText::new(model::headline(e.ui))
                        .size(13.5)
                        .color(pal.text_strong),
                )
                .wrap(),
            );
            ui.add_enabled_ui(!sending, |ui| {
                if let Some(a) = controls(ui, e.ui, draft) {
                    action = Some(answer(a));
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .link(RichText::new("Open session").size(12.0))
                    .on_hover_text("Show this session's tab")
                    .clicked()
                {
                    action = Some(Action::Open(target.clone()));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if DraftButton::new("Cancel")
                        .enabled(!sending)
                        .min_width(72.0)
                        .show(ui)
                        .on_hover_text("Dismiss the dialog (omp sees a cancel)")
                        .clicked()
                    {
                        action = Some(answer(UiAnswer::Cancel));
                    }
                    if sending {
                        ui.add(crate::widgets::Spin::new(14.0, pal.accent));
                    }
                });
            });
        });
    action
}

/// The answer controls of one dialog kind; returns the answer once the user gave it.
fn controls(ui: &mut Ui, req: &PendingUi, draft: &mut Draft) -> Option<UiAnswer> {
    let pal = Palette::of_ui(ui);
    match req.kind {
        PendingUiKind::Select => {
            if let Some(m) = req.message.as_deref().filter(|m| !m.trim().is_empty()) {
                note(ui, m);
            }
            let mut picked = None;
            for (i, o) in req.options.iter().enumerate() {
                let index = i as u32;
                let icon = if req.multi {
                    ICON_SQUARE
                } else if req.initial == Some(index) {
                    ICON_CIRCLE_DOT
                } else {
                    ICON_CIRCLE
                };
                if option_row(ui, icon, &o.label, o.description.as_deref(), false).clicked() {
                    picked = model::select_answer(req, index);
                }
            }
            if req.multi {
                hint(
                    ui,
                    "Each click toggles one option; omp asks again until you finish.",
                );
            }
            picked
        }
        PendingUiKind::Confirm => {
            if let Some(m) = req.message.as_deref().filter(|m| !m.trim().is_empty()) {
                note(ui, m);
            }
            let mut out = None;
            ui.horizontal(|ui| {
                if DraftButton::new("Yes")
                    .kind(DraftKind::Primary)
                    .min_width(72.0)
                    .show(ui)
                    .clicked()
                {
                    out = Some(UiAnswer::Confirm { yes: true });
                }
                if DraftButton::new("No").min_width(72.0).show(ui).clicked() {
                    out = Some(UiAnswer::Confirm { yes: false });
                }
            });
            out
        }
        PendingUiKind::Input | PendingUiKind::Editor => {
            let Draft::Text(text) = draft else {
                return None;
            };
            let editor = req.kind == PendingUiKind::Editor;
            let edit = if editor {
                egui::TextEdit::multiline(text)
                    .desired_rows(6)
                    .font(FontId::new(12.5, FontFamily::Monospace))
            } else {
                egui::TextEdit::singleline(text)
                    .hint_text(req.message.as_deref().unwrap_or("Your answer"))
            };
            let resp = ui.add(edit.desired_width(f32::INFINITY));
            let enter =
                !editor && resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let send = send_button(ui, true);
            (send || enter).then(|| model::text_answer(req.kind, text))
        }
        PendingUiKind::Ask => {
            let Draft::Ask(drafts) = draft else {
                return None;
            };
            for (q, d) in req.questions.iter().zip(drafts.iter_mut()) {
                ask_question(ui, q, d);
            }
            let answer = model::ask_answer(&req.questions, drafts);
            send_button(ui, answer.is_some())
                .then_some(answer)
                .flatten()
        }
        PendingUiKind::Approve => {
            let a = req.approve.as_ref()?;
            let shown = if a.full.trim().is_empty() {
                &a.compact
            } else {
                &a.full
            };
            Frame::NONE
                .fill(pal.panel)
                .stroke(Stroke::new(1.0, pal.stroke))
                .corner_radius(theme::ui_rad(6))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ScrollArea::vertical()
                        .id_salt(("approve", req.id))
                        .max_height(140.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(shown.as_str())
                                        .font(FontId::new(12.0, FontFamily::Monospace))
                                        .color(pal.text),
                                )
                                .wrap(),
                            );
                        });
                });
            let mut out = None;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
                for (label, decision, tone) in model::approve_choices(a) {
                    let kind = match tone {
                        Tone::Primary => DraftKind::Primary,
                        Tone::Plain => DraftKind::Secondary,
                        Tone::Danger => DraftKind::Danger,
                    };
                    if DraftButton::new(label).kind(kind).show(ui).clicked() {
                        out = Some(UiAnswer::Approve { decision });
                    }
                }
            });
            out
        }
        PendingUiKind::Form => {
            let (Some(form), Draft::Form(drafts)) = (req.form.as_ref(), draft) else {
                return None;
            };
            let headline = model::headline(req);
            for (s, section) in form.sections.iter().enumerate() {
                form_section(ui, req.id, s, section, &form.fields, drafts, headline);
            }
            let answer = model::form_answer(&form.fields, drafts);
            send_button(ui, answer.is_some())
                .then_some(answer)
                .flatten()
        }
    }
}

/// A right-aligned Send, one row tall (a bare right-to-left layout would take all the height
/// the popover offers and centre the button in it).
fn send_button(ui: &mut Ui, enabled: bool) -> bool {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            DraftButton::new("Send")
                .kind(DraftKind::Primary)
                .enabled(enabled)
                .min_width(72.0)
                .show(ui)
                .clicked()
        })
        .inner
    })
    .inner
}

fn note(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add(egui::Label::new(RichText::new(text).size(12.5).color(pal.text)).wrap());
}

fn hint(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add(egui::Label::new(RichText::new(text).size(11.0).color(pal.weak)).wrap());
}

/// A clickable option row: marker icon, label, optional description; the outline pops on hover.
fn option_row(
    ui: &mut Ui,
    icon: crate::icons::Icon,
    label: &str,
    description: Option<&str>,
    on: bool,
) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let width = ui.available_width();
    let text_w = width - 34.0;
    let title = ui
        .painter()
        .layout(label.to_owned(), theme::font(12.5), pal.text_strong, text_w);
    let desc = description.filter(|d| !d.trim().is_empty()).map(|d| {
        ui.painter()
            .layout(d.to_owned(), theme::font(11.0), pal.weak, text_w)
    });
    let h = 10.0 + title.size().y + desc.as_ref().map_or(0.0, |d| d.size().y + 2.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h.max(28.0)), Sense::click());
    let enabled = ui.is_enabled();
    let hov = if enabled {
        ui.ctx()
            .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1)
    } else {
        0.0
    };
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let r = theme::ui_rad(6);
        p.rect_filled(rect, r, theme::hover_fill(pal).gamma_multiply(0.6 * hov));
        p.rect_stroke(
            rect,
            r,
            Stroke::new(
                1.0,
                theme::mix(pal.stroke, pal.accent, if on { 1.0 } else { hov }),
            ),
            StrokeKind::Inside,
        );
        let tint = if on { pal.accent } else { pal.weak };
        paint_icon(
            p,
            egui::pos2(rect.left() + 15.0, rect.top() + 5.0 + title.size().y / 2.0),
            Align2::CENTER_CENTER,
            icon,
            14.0,
            if enabled {
                tint
            } else {
                tint.gamma_multiply(0.5)
            },
        );
        let x = rect.left() + 28.0;
        let title_h = title.size().y;
        p.galley(egui::pos2(x, rect.top() + 5.0), title, pal.text_strong);
        if let Some(d) = desc {
            p.galley(egui::pos2(x, rect.top() + 7.0 + title_h), d, pal.weak);
        }
    }
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// One question of an `ask`: header, question, options (recommended marked), own answer.
fn ask_question(ui: &mut Ui, q: &UiQuestion, d: &mut model::AskDraft) {
    let pal = Palette::of_ui(ui);
    ui.horizontal_wrapped(|ui| {
        if let Some(h) = q.header.as_deref().filter(|h| !h.trim().is_empty()) {
            badge::show(ui, None, h, Hue::Blue, 10.5);
        }
        ui.add(egui::Label::new(RichText::new(&q.question).size(12.5).color(pal.text)).wrap());
    });
    let mut clicked = None;
    for (i, o) in q.options.iter().enumerate() {
        let index = i as u32;
        let on = d.selected.contains(&index);
        let icon = match (q.multi, on) {
            (true, true) => ICON_CIRCLE_CHECK,
            (true, false) => ICON_SQUARE,
            (false, true) => ICON_CIRCLE_DOT,
            (false, false) => ICON_CIRCLE,
        };
        let label = if q.recommended == Some(index) {
            format!("{} (recommended)", o.label)
        } else {
            o.label.clone()
        };
        if option_row(ui, icon, &label, o.description.as_deref(), on).clicked() {
            clicked = Some(index);
        }
    }
    if let Some(index) = clicked {
        model::toggle_ask(q, d, index);
    }
    ui.add(
        egui::TextEdit::singleline(&mut d.custom)
            .hint_text("Or type your own answer")
            .desired_width(f32::INFINITY),
    );
}

/// A form section: its legend, then text / list / pre / table / image blocks and fields. A
/// legend repeating the card's headline, and a field label repeating the legend, are not
/// shown twice.
fn form_section(
    ui: &mut Ui,
    request: u64,
    s: usize,
    section: &UiFormSection,
    fields: &[UiFormField],
    drafts: &mut [FormDraft],
    headline: &str,
) {
    let pal = Palette::of_ui(ui);
    if !section.title.is_empty() && !spans_are(&section.title, headline) {
        ui.label(spans(ui, &section.title, 13.0, pal.text_strong, true));
    }
    for (b, block) in section.blocks.iter().enumerate() {
        match block {
            UiFormBlock::Text { spans: t, heading } => {
                let (size, color) = if *heading {
                    (13.0, pal.text_strong)
                } else {
                    (12.5, pal.text)
                };
                ui.add(egui::Label::new(spans(ui, t, size, color, *heading)).wrap());
            }
            UiFormBlock::List { items, ordered } => {
                for (n, item) in items.iter().enumerate() {
                    ui.horizontal_wrapped(|ui| {
                        let marker = if *ordered {
                            format!("{}.", n + 1)
                        } else {
                            "•".to_owned()
                        };
                        ui.label(RichText::new(marker).color(pal.weak));
                        ui.add(egui::Label::new(spans(ui, item, 12.5, pal.text, false)).wrap());
                    });
                }
            }
            UiFormBlock::Pre { text } => {
                Frame::NONE
                    .fill(pal.panel)
                    .corner_radius(theme::ui_rad(6))
                    .inner_margin(Margin::same(6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(
                                RichText::new(text)
                                    .font(FontId::new(12.0, FontFamily::Monospace))
                                    .color(pal.text),
                            )
                            .wrap(),
                        );
                    });
            }
            UiFormBlock::Table { rows, header_rows } => {
                egui::Grid::new(("form-table", request, s, b))
                    .striped(true)
                    .spacing(vec2(10.0, 4.0))
                    .show(ui, |ui| {
                        for (r, row) in rows.iter().enumerate() {
                            let head = (r as u32) < *header_rows;
                            for cell in row {
                                ui.label(spans(
                                    ui,
                                    cell,
                                    12.0,
                                    if head { pal.text_strong } else { pal.text },
                                    head,
                                ));
                            }
                            ui.end_row();
                        }
                    });
            }
            UiFormBlock::Image { alt } => {
                ui.label(
                    RichText::new(format!("[image: {alt}]"))
                        .italics()
                        .color(pal.weak),
                );
            }
            UiFormBlock::Field { field } => {
                let i = *field as usize;
                if let (Some(f), Some(d)) = (fields.get(i), drafts.get_mut(i)) {
                    form_field(ui, (request, i), f, d, &section.title);
                }
            }
            UiFormBlock::Other => {}
        }
    }
}

/// One form field: label, its control, and why omp would refuse it.
fn form_field(
    ui: &mut Ui,
    id: (u64, usize),
    field: &UiFormField,
    draft: &mut FormDraft,
    legend: &[UiSpan],
) {
    let pal = Palette::of_ui(ui);
    match field {
        UiFormField::Choice {
            label,
            question,
            multi,
            select,
            other,
            required,
            options,
            ..
        } => {
            field_label(ui, label, question, *required, legend);
            let checked: Vec<u32> = match &*draft {
                FormDraft::Choice { checked, .. } => checked.clone(),
                FormDraft::Text(_) => return,
            };
            if *select && !multi {
                let current = checked
                    .first()
                    .and_then(|&i| options.get(i as usize))
                    .map_or_else(|| "Choose…".to_owned(), |o| plain(&o.label));
                let mut pick = None;
                egui::ComboBox::from_id_salt(("form-select", id))
                    .selected_text(current)
                    .width(ui.available_width() - 8.0)
                    .show_ui(ui, |ui| {
                        for (i, o) in options.iter().enumerate() {
                            let on = checked.first() == Some(&(i as u32));
                            if ui
                                .add_enabled(
                                    !o.disabled,
                                    egui::Button::selectable(on, plain(&o.label)),
                                )
                                .clicked()
                            {
                                pick = Some(i as u32);
                            }
                        }
                    });
                if let Some(i) = pick {
                    model::toggle_form_option(field, draft, i);
                }
            } else {
                let mut clicked = None;
                for (i, o) in options.iter().enumerate() {
                    let index = i as u32;
                    let on = checked.contains(&index);
                    let icon = match (multi, on) {
                        (true, true) => ICON_CIRCLE_CHECK,
                        (true, false) => ICON_SQUARE,
                        (false, true) => ICON_CIRCLE_DOT,
                        (false, false) => ICON_CIRCLE,
                    };
                    let resp = ui
                        .add_enabled_ui(!o.disabled, |ui| {
                            option_row(ui, icon, &plain(&o.label), None, on)
                        })
                        .inner;
                    if resp.clicked() {
                        clicked = Some(index);
                    }
                }
                if let Some(i) = clicked {
                    model::toggle_form_option(field, draft, i);
                }
            }
            if *other {
                let FormDraft::Choice {
                    other: other_text,
                    other_picked,
                    ..
                } = draft
                else {
                    return;
                };
                let mut pick_other = false;
                ui.horizontal(|ui| {
                    if !multi {
                        let icon = if *other_picked {
                            ICON_CIRCLE_DOT
                        } else {
                            ICON_CIRCLE
                        };
                        if ui
                            .add(egui::Button::new(icon_text(icon, 13.0, pal.weak)).frame(false))
                            .clicked()
                        {
                            pick_other = true;
                        }
                    }
                    let resp = ui.add(
                        egui::TextEdit::singleline(other_text)
                            .hint_text("Other")
                            .desired_width(f32::INFINITY),
                    );
                    if resp.changed() && !multi {
                        pick_other = true;
                    }
                });
                if pick_other {
                    model::pick_form_other(field, draft);
                }
            }
        }
        UiFormField::Text {
            label,
            question,
            multiline,
            placeholder,
            required,
            ..
        } => {
            field_label(ui, label, question, *required, legend);
            let FormDraft::Text(text) = draft else {
                return;
            };
            let edit = if *multiline {
                egui::TextEdit::multiline(text).desired_rows(3)
            } else {
                egui::TextEdit::singleline(text)
            };
            ui.add(
                edit.hint_text(placeholder.as_str())
                    .desired_width(f32::INFINITY),
            );
        }
    }
    if let Some(err) = model::form_field_error(field, Some(draft)) {
        ui.label(RichText::new(err).size(11.0).color(pal.warn));
    }
}

fn field_label(ui: &mut Ui, label: &[UiSpan], question: &str, required: bool, legend: &[UiSpan]) {
    let pal = Palette::of_ui(ui);
    // The legend right above already says it.
    let repeats = !legend.is_empty()
        && if label.is_empty() {
            spans_are(legend, question)
        } else {
            span_chars(label).eq(span_chars(legend))
        };
    if repeats && !required {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        if !repeats {
            if label.is_empty() {
                ui.label(RichText::new(question).size(12.5).color(pal.text));
            } else {
                ui.label(spans(ui, label, 12.5, pal.text, false));
            }
        }
        if required {
            ui.label(RichText::new("*").color(pal.warn));
        }
    });
}

/// Whether `spans` read exactly `text` (both trimmed), without building the string.
fn spans_are(spans: &[UiSpan], text: &str) -> bool {
    span_chars(spans).eq(text.trim().chars())
}

/// The spans' text as one trimmed run of chars.
fn span_chars(spans: &[UiSpan]) -> impl Iterator<Item = char> + '_ {
    let all = || spans.iter().flat_map(|s| s.text.chars());
    let lead = all().take_while(|c| c.is_whitespace()).count();
    let end = all()
        .enumerate()
        .filter(|(_, c)| !c.is_whitespace())
        .last()
        .map_or(0, |(i, _)| i + 1);
    all().skip(lead).take(end.saturating_sub(lead))
}

/// Spans as one laid-out text: `code` monospace, `strong` bold-coloured, `em` italic.
fn spans(ui: &Ui, spans: &[UiSpan], size: f32, color: Color32, strong: bool) -> LayoutJob {
    let pal = Palette::of_ui(ui);
    let mut job = LayoutJob::default();
    for s in spans {
        let mut fmt = TextFormat {
            font_id: theme::font(size),
            color: if strong { pal.text_strong } else { color },
            ..TextFormat::default()
        };
        match s.style.as_deref() {
            Some("code") => {
                fmt.font_id = FontId::new(size - 0.5, FontFamily::Monospace);
                fmt.background = pal.panel;
            }
            Some("strong") => fmt.color = pal.text_strong,
            Some("em") => fmt.italics = true,
            _ => {}
        }
        job.append(&s.text, 0.0, fmt);
    }
    job
}

/// Spans as plain text (option labels, dropdown entries).
fn plain(spans: &[UiSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}
