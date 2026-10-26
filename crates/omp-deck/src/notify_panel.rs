//! The title bar's notification bell (count badge) and its inbox popover.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use egui::{
    Align, Align2, Color32, CursorIcon, Galley, Id, Layout, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, TextStyle, TextWrapMode, Ui, WidgetText, pos2, vec2,
};

use crate::badge::{self, Hue};
use crate::icons::{
    ICON_BELL, ICON_CIRCLE_ALERT, ICON_CIRCLE_CHECK, ICON_CIRCLE_X, ICON_POWER, ICON_SETTINGS,
    ICON_X, Icon,
};
use crate::sidebar::format_age;
use crate::theme::{self, Palette};
use crate::widgets::{DraftButton, icon_button, icon_text, paint_icon};
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::InboxEntry;
use deck_service::notifications::inbox::Kind;

const BELL_ID: &str = "notify-bell";
/// Popover width.
const WIDTH: f32 = 360.0;
/// The popover grows with its rows up to this share of the window height.
const MAX_HEIGHT_SHARE: f32 = 0.6;
const PAD: f32 = 10.0;
const ICON_W: f32 = 22.0;
/// Height assumed for an off-screen row whose text was never laid out (one-line body).
const ESTIMATED_ROW_H: f32 = 72.0;

/// What the popover asks the app to do.
#[derive(Debug, PartialEq, Eq)]
pub enum PanelAction {
    /// Activate the tab, raise the window, drop the entry.
    Open(SessionTarget),
    Dismiss(SessionTarget),
    /// Close the popover and open Settings → OmpDeck → Notifications.
    Settings,
    ClearAll,
}

pub struct PanelInput<'a> {
    pub entries: &'a [InboxEntry],
    /// Badge text; `None` while the inbox is empty.
    pub badge: Option<&'a str>,
    /// An entry needs the user (error / attention): the badge uses the warn colour.
    pub warn: bool,
    pub now: SystemTime,
}

/// The popover's id in egui's popup memory (for opening it from the palette).
pub fn popup_id() -> Id {
    Id::unique(BELL_ID).with("popup")
}

/// Width of [`bell`].
pub fn bell_width() -> f32 {
    theme::dens(28.0, 32.0)
}

/// The bell button with its badge, and the popover while it is open.
pub fn bell(ui: &mut Ui, input: &PanelInput<'_>) -> Option<PanelAction> {
    let pal = Palette::of_ui(ui);
    let side = bell_width();
    let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    let resp = ui.interact(rect, Id::unique(BELL_ID), Sense::click());
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
        paint_icon(
            p,
            rect.center(),
            Align2::CENTER_CENTER,
            ICON_BELL,
            size,
            theme::mix(pal.weak, pal.text_strong, hov),
        );
        if let Some(text) = input.badge {
            paint_badge(ui, rect, text, input.warn, pal);
        }
    }
    let resp = resp
        .on_hover_text("Notifications")
        .on_hover_cursor(CursorIcon::PointingHand);
    theme::dropdown_menu(&resp, WIDTH, |ui| popover(ui, input)).and_then(|r| r.inner)
}

/// Count pill on the bell's top-right corner, in the warn or accent colour.
fn paint_badge(ui: &Ui, bell: Rect, text: &str, warn: bool, pal: &Palette) {
    let fill = if warn { pal.warn } else { pal.accent };
    let ink = if luma(fill) > 150.0 {
        egui::Color32::BLACK
    } else {
        egui::Color32::WHITE
    };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::font(10.0), ink);
    let size = vec2((galley.size().x + 8.0).max(15.0), 15.0);
    let rect = Rect::from_min_size(pos2(bell.right() - size.x + 3.0, bell.top() - 1.0), size);
    let p = ui.painter();
    p.rect(
        rect,
        8,
        fill,
        Stroke::new(1.5, pal.bar),
        StrokeKind::Outside,
    );
    p.galley(rect.center() - galley.size() / 2.0, galley, ink);
}

fn luma(c: egui::Color32) -> f32 {
    0.299 * f32::from(c.r()) + 0.587 * f32::from(c.g()) + 0.114 * f32::from(c.b())
}

fn popover(ui: &mut Ui, input: &PanelInput<'_>) -> Option<PanelAction> {
    let pal = Palette::of_ui(ui);
    let mut action = None;
    ui.set_width(WIDTH);
    ui.spacing_mut().item_spacing.y = 6.0;
    ui.horizontal(|ui| {
        ui.add_space(PAD - 2.0);
        ui.spacing_mut().item_spacing.x = 10.0;
        crate::card::icon_tile(ui, 30.0, ICON_BELL, Color32::TRANSPARENT, true);
        ui.label(
            theme::strong("Notifications")
                .size(14.5)
                .color(pal.text_strong),
        );
        if !input.entries.is_empty() {
            badge::show(ui, None, &input.entries.len().to_string(), Hue::Amber, 11.0);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(PAD - 2.0);
            let clear = DraftButton::new("Clear all")
                .enabled(!input.entries.is_empty())
                .min_width(76.0)
                .show(ui);
            if clear.clicked() {
                action = Some(PanelAction::ClearAll);
            }
            if icon_button(ui, ICON_SETTINGS, "Notification settings", false).clicked() {
                action = Some(PanelAction::Settings);
                ui.close();
            }
        });
    });
    let left = ui.min_rect().left();
    crate::card::header_rule(
        ui,
        left,
        left + WIDTH,
        ui.cursor().top(),
        Color32::TRANSPARENT,
    );
    if input.entries.is_empty() {
        ui.add_space(14.0);
        ui.vertical_centered(|ui| {
            ui.label(icon_text(ICON_CIRCLE_CHECK, 26.0, pal.ok));
            ui.add_space(2.0);
            ui.label(RichText::new("You're all caught up").color(pal.weak));
        });
        ui.add_space(16.0);
        return action;
    }
    let max_h = ui.ctx().content_rect().height() * MAX_HEIGHT_SHARE;
    ScrollArea::vertical()
        .max_height(max_h)
        .auto_shrink([false, true])
        .show_viewport(ui, |ui, viewport| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let width = ui.available_width();
            let text_w = width - 2.0 * PAD - ICON_W - 22.0;
            let key = LayoutKey {
                width: width.to_bits(),
                pixels_per_point: ui.ctx().pixels_per_point().to_bits(),
                colors: [pal.text_strong, pal.weak, pal.text],
                font: theme::ui_font(),
            };
            let id = popup_id().with("rows");
            let mut cache: RowCache = ui.ctx().data_mut(|d| d.remove_temp(id)).unwrap_or_default();
            if cache.key != Some(key) {
                cache.rows.clear();
                cache.key = Some(key);
            }
            cache
                .rows
                .retain(|uid, _| input.entries.iter().any(|e| e.target == *uid));
            let spacing = ui.spacing().item_spacing.y;
            let mut y = 0.0_f32;
            for entry in input.entries {
                let known = cache
                    .rows
                    .get(&entry.target)
                    .filter(|c| c.entry == *entry)
                    .map(|c| c.height);
                let guess = known.unwrap_or(ESTIMATED_ROW_H);
                if y + guess < viewport.min.y || y > viewport.max.y {
                    // Off screen: keep its place without laying its text out.
                    ui.allocate_exact_size(vec2(width, guess), Sense::hover());
                    y += guess + spacing;
                    continue;
                }
                refresh_row(ui, &mut cache, entry, input.now, pal, text_w);
                let Some(cached) = cache.rows.get(&entry.target) else {
                    continue;
                };
                y += cached.height + spacing;
                if let Some(a) = row(ui, entry, cached, pal) {
                    action = Some(a);
                    ui.close();
                }
            }
            ui.ctx().data_mut(|d| d.insert_temp(id, cache));
        });
    action
}

fn kind_icon(kind: &Kind, pal: &Palette) -> (Icon, egui::Color32) {
    match kind {
        Kind::Finished => (ICON_CIRCLE_CHECK, pal.ok),
        Kind::Error(_) => (ICON_CIRCLE_X, pal.danger),
        Kind::Attention => (ICON_CIRCLE_ALERT, pal.warn),
        Kind::Exited { code: Some(0) } => (ICON_POWER, pal.weak),
        Kind::Exited { .. } => (ICON_POWER, pal.danger),
    }
}

/// `"now"` → `"just now"`, `"5m"` → `"5m ago"`.
fn age_label(now: SystemTime, at: SystemTime) -> String {
    match format_age(now, at).as_str() {
        "now" => "just now".to_owned(),
        age => format!("{age} ago"),
    }
}

fn galley(
    ui: &Ui,
    text: impl Into<WidgetText>,
    wrap: TextWrapMode,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    text.into()
        .into_galley(ui, Some(wrap), width, TextStyle::Body)
}

/// What a row's galleys were laid out for; any change relays them out.
#[derive(Clone, Copy, PartialEq)]
struct LayoutKey {
    width: u32,
    pixels_per_point: u32,
    colors: [egui::Color32; 3],
    font: deck_core::config::UiFont,
}

/// One entry's laid-out text. `entry` is the revision it was built from (an entry is replaced as
/// a whole when its tab has a newer event); `age_minutes` is the bucket the sub line shows.
#[derive(Clone)]
struct CachedRow {
    entry: InboxEntry,
    age_minutes: u64,
    title: Arc<Galley>,
    sub: Arc<Galley>,
    body: Arc<Galley>,
    height: f32,
}

#[derive(Clone, Default)]
struct RowCache {
    key: Option<LayoutKey>,
    rows: HashMap<SessionTarget, CachedRow>,
}

fn age_minutes(now: SystemTime, at: SystemTime) -> u64 {
    now.duration_since(at).map_or(0, |d| d.as_secs() / 60)
}

/// When the gateway recorded the entry.
fn entry_time(e: &InboxEntry) -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(e.at_unix_ms)
}

fn sub_galley(ui: &Ui, e: &InboxEntry, now: SystemTime, pal: &Palette, text_w: f32) -> Arc<Galley> {
    let mut sub_text = String::new();
    if !e.project.is_empty() {
        sub_text.push_str(&e.project);
        sub_text.push_str(" · ");
    }
    sub_text.push_str(&age_label(now, entry_time(e)));
    galley(
        ui,
        RichText::new(sub_text).size(11.5).color(pal.weak),
        TextWrapMode::Truncate,
        text_w,
    )
}

fn row_height(title: &Galley, sub: &Galley, body: &Galley) -> f32 {
    2.0 * PAD + title.size().y + sub.size().y + body.size().y + 4.0
}

/// Brings `cache.rows[e.tab]` up to date for `e`: a full layout for a new revision, only the
/// sub line when just the age's minute moved on.
fn refresh_row(
    ui: &Ui,
    cache: &mut RowCache,
    e: &InboxEntry,
    now: SystemTime,
    pal: &Palette,
    text_w: f32,
) {
    let minutes = age_minutes(now, entry_time(e));
    if let Some(c) = cache.rows.get_mut(&e.target)
        && c.entry == *e
    {
        if c.age_minutes != minutes {
            c.sub = sub_galley(ui, e, now, pal, text_w);
            c.age_minutes = minutes;
            c.height = row_height(&c.title, &c.sub, &c.body);
        }
        return;
    }
    let title = galley(
        ui,
        theme::strong(&e.title).size(13.5).color(pal.text_strong),
        TextWrapMode::Truncate,
        text_w,
    );
    let sub = sub_galley(ui, e, now, pal, text_w);
    let body = galley(
        ui,
        RichText::new(&e.body).size(12.5).color(pal.text),
        TextWrapMode::Wrap,
        text_w,
    );
    let height = row_height(&title, &sub, &body);
    cache.rows.insert(
        e.target.clone(),
        CachedRow {
            entry: e.clone(),
            age_minutes: minutes,
            title,
            sub,
            body,
            height,
        },
    );
}

/// One entry: kind icon, bold tab title, `project · age`, the body; a ✕ while hovered.
fn row(ui: &mut Ui, e: &InboxEntry, c: &CachedRow, pal: &Palette) -> Option<PanelAction> {
    let width = ui.available_width();
    let (icon, tint) = kind_icon(&e.kind, pal);
    let (title, sub, body) = (c.title.clone(), c.sub.clone(), c.body.clone());
    let height = c.height;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let resp = ui.interact(rect, Id::unique(("notify-row", &e.target)), Sense::click());
    let x_rect = Rect::from_min_size(
        pos2(rect.right() - PAD - 18.0, rect.top() + PAD - 2.0),
        vec2(20.0, 20.0),
    );
    let x = ui.interact(
        x_rect,
        Id::unique(("notify-row-x", &e.target)),
        Sense::click(),
    );
    let hovered = resp.hovered() || x.hovered();
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), hovered, 0.1);
    let p = ui.painter();
    let radius = if theme::flat() { 8 } else { 12 };
    p.rect(
        rect,
        radius,
        theme::mix(pal.panel, theme::hover_fill(pal), hov),
        Stroke::new(
            1.0 + 0.5 * hov,
            theme::mix(pal.stroke, theme::hover_stroke(pal), hov),
        ),
        StrokeKind::Inside,
    );
    paint_icon(
        p,
        pos2(rect.left() + PAD + ICON_W / 2.0, rect.top() + PAD + 9.0),
        Align2::CENTER_CENTER,
        icon,
        17.0,
        tint,
    );
    let left = rect.left() + PAD + ICON_W;
    let mut y = rect.top() + PAD;
    let title_h = title.size().y;
    p.galley(pos2(left, y), title, pal.text_strong);
    y += title_h;
    let sub_h = sub.size().y;
    p.galley(pos2(left, y), sub, pal.weak);
    y += sub_h + 4.0;
    p.galley(pos2(left, y), body, pal.text);
    if hov > 0.01 {
        if x.hovered() {
            p.rect_filled(x_rect, 5, theme::hover_fill(pal));
        }
        paint_icon(
            p,
            x_rect.center(),
            Align2::CENTER_CENTER,
            ICON_X,
            14.0,
            theme::mix(
                pal.weak,
                pal.text_strong,
                if x.hovered() { 1.0 } else { 0.0 },
            )
            .gamma_multiply(hov),
        );
    }
    if x.clicked() {
        Some(PanelAction::Dismiss(e.target.clone()))
    } else if resp.clicked() {
        Some(PanelAction::Open(e.target.clone()))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ages_read_as_ago_with_a_just_now_floor() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        assert_eq!(age_label(now, now), "just now");
        assert_eq!(age_label(now, now - Duration::from_secs(300)), "5m ago");
    }
}
