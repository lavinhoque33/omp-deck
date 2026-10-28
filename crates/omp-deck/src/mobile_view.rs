//! Mobile access in the settings: the relay, this desktop's fingerprint, the paired devices and the
//! "Pair a device" entry, plus the pure helpers the pairing dialog shares (link wording, last
//! seen, QR module grid, countdown). Everything here only draws and reports a [`MobileAction`];
//! the app (`app/mobile.rs`) owns the dialogs and talks to the gateway.

use egui::{
    Align, Color32, Layout, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder,
    Vec2, pos2, vec2,
};

use deck_service::gateway::protocol::{DeviceView, MobileView, Permissions, RelayLink};

use crate::badge::{self, Hue};
use crate::card;

use crate::icons::{
    ICON_GLOBE, ICON_KEY_ROUND, ICON_PENCIL, ICON_PLUS, ICON_REFRESH_CW, ICON_TRASH_2, ICON_X, Icon,
};
use crate::theme::{self, Palette};
use crate::tree_layout::age;
use crate::widgets::{DraftButton, DraftKind, icon_button, menu_item};

const KEY_W: f32 = 96.0;

/// "Reconnect" applies: a relay is set, Mobile access is on, and the link is refused or waiting
/// in backoff.
pub fn can_reconnect(view: Option<&MobileView>, mode_off: bool) -> bool {
    view.is_some_and(|v| {
        v.relay.is_some()
            && !mode_off
            && matches!(
                v.link,
                RelayLink::Refused { .. } | RelayLink::Connecting { attempt: 1.., .. }
            )
    })
}

/// What the card needs besides the gateway's record.
pub struct MobileInput<'a> {
    /// `None` until the gateway reported mobile access.
    pub view: Option<&'a MobileView>,
    /// The gateway's lifetime mode is Off: the relay link cannot run.
    pub mode_off: bool,
    pub gateway_up: bool,
    /// Now, unix milliseconds (relative times).
    pub now_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MobileAction {
    /// "Set relay…" / "Change…".
    EditRelay,
    ClearRelay,
    /// Restart the relay link now.
    Reconnect,
    Pair,
    /// Device id.
    Rename(String),
    Permissions(String),
    Revoke(String),
}

// ------------------------------------------------------------------ wording

/// How the relay link reads in the card.
#[derive(Debug, PartialEq, Eq)]
pub struct LinkLabel {
    pub text: String,
    pub hue: Hue,
    pub detail: Option<String>,
}

pub fn link_label(link: &RelayLink, mode_off: bool) -> LinkLabel {
    match link {
        RelayLink::Off => LinkLabel {
            text: "Off".to_owned(),
            hue: Hue::Amber,
            detail: mode_off.then(|| "Turn on Mobile access to connect".to_owned()),
        },
        RelayLink::Connecting { attempt, error } => LinkLabel {
            text: format!("Connecting, attempt {attempt}"),
            hue: Hue::Amber,
            detail: error.clone(),
        },
        RelayLink::Up => LinkLabel {
            text: "Connected".to_owned(),
            hue: Hue::Green,
            detail: None,
        },
        RelayLink::Refused { reason } => LinkLabel {
            text: "Refused".to_owned(),
            hue: Hue::Rose,
            detail: Some(reason.clone()),
        },
    }
}

/// Why "Pair a device" is unavailable.
#[derive(Debug, PartialEq, Eq)]
pub enum PairBlock {
    GatewayDown,
    NoReport,
    NoRelay,
    ModeOff,
    NotRunning,
    Connecting,
    Refused(String),
    AlreadyOpen,
}

impl PairBlock {
    /// Full sentence (tooltip, toast).
    pub fn text(&self) -> String {
        match self {
            PairBlock::Refused(reason) => format!("The relay refused the connection: {reason}"),
            other => other.short().to_owned(),
        }
    }

    /// Reason without the dynamic part (palette rows).
    pub fn short(&self) -> &'static str {
        match self {
            PairBlock::GatewayDown => "The gateway is not reachable right now.",
            PairBlock::NoReport => "Waiting for the gateway to report mobile access.",
            PairBlock::NoRelay => "Set a relay first.",
            PairBlock::ModeOff => "Turn on Mobile access first.",
            PairBlock::NotRunning => "The relay link is not running yet.",
            PairBlock::Connecting => "Waiting for the relay connection…",
            PairBlock::Refused(_) => "The relay refused the connection.",
            PairBlock::AlreadyOpen => "A pairing is already open.",
        }
    }
}

/// What stops "Pair a device", or `None` when it can be used.
pub fn pair_block(
    view: Option<&MobileView>,
    mode_off: bool,
    gateway_up: bool,
) -> Option<PairBlock> {
    if !gateway_up {
        return Some(PairBlock::GatewayDown);
    }
    let Some(view) = view else {
        return Some(PairBlock::NoReport);
    };
    if view.relay.is_none() {
        return Some(PairBlock::NoRelay);
    }
    match &view.link {
        RelayLink::Up => view.pairing.is_some().then_some(PairBlock::AlreadyOpen),
        RelayLink::Off if mode_off => Some(PairBlock::ModeOff),
        RelayLink::Off => Some(PairBlock::NotRunning),
        RelayLink::Connecting { .. } => Some(PairBlock::Connecting),
        RelayLink::Refused { reason } => Some(PairBlock::Refused(reason.clone())),
    }
}

/// `online`, `last seen 5m ago`, `last seen just now` or `never connected`.
pub fn presence_text(d: &DeviceView, now_ms: u64) -> String {
    if d.online {
        return "online".to_owned();
    }
    if d.last_seen_unix_ms == 0 {
        return "never connected".to_owned();
    }
    let a = age((now_ms / 1000) as i64, (d.last_seen_unix_ms / 1000) as i64);
    if a == "now" {
        "last seen just now".to_owned()
    } else {
        format!("last seen {a} ago")
    }
}

/// `m:ss` until `expires_ms` (`0:00` once past).
pub fn countdown(expires_ms: u64, now_ms: u64) -> String {
    let s = expires_ms.saturating_sub(now_ms).div_ceil(1000);
    format!("{}:{:02}", s / 60, s % 60)
}

// ------------------------------------------------------------------ pairing end

/// What the dialog last showed of a pairing, to tell how it ended once the topic drops it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingSeen {
    pub id: String,
    pub expires_ms: u64,
    /// `(name, fingerprint)` once a device asked.
    pub approval: Option<(String, String)>,
}

/// What this window itself did to the pairing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalEnd {
    Approved,
    Denied,
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairingEnd {
    Paired(String),
    Denied,
    Cancelled,
    Expired,
    /// Closed by something else (another window's cancel or deny, the gateway).
    Closed,
}

pub fn pairing_end(
    seen: &PairingSeen,
    local: Option<LocalEnd>,
    devices: &[DeviceView],
    now_ms: u64,
) -> PairingEnd {
    if let Some((name, fingerprint)) = &seen.approval
        && (local == Some(LocalEnd::Approved)
            || devices.iter().any(|d| &d.fingerprint == fingerprint))
    {
        return PairingEnd::Paired(name.clone());
    }
    match local {
        Some(LocalEnd::Denied) => PairingEnd::Denied,
        Some(LocalEnd::Cancelled) => PairingEnd::Cancelled,
        _ if now_ms >= seen.expires_ms => PairingEnd::Expired,
        _ => PairingEnd::Closed,
    }
}

// ------------------------------------------------------------------ QR code

/// Modules of quiet zone around the code (the QR specification asks for four).
pub const QR_QUIET: usize = 4;
/// The code's smallest painted side, in points.
const QR_MIN_POINTS: f32 = 240.0;

/// A QR code's dark/light modules.
pub struct QrGrid {
    size: usize,
    dark: Vec<bool>,
}

impl Drop for QrGrid {
    /// The modules encode the pairing link: wipe them with it.
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.dark);
    }
}

impl QrGrid {
    /// Encodes `text` with medium error correction.
    pub fn encode(text: &str) -> Result<QrGrid, String> {
        let qr = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium)
            .map_err(|_| "The link is too long for a QR code.".to_owned())?;
        let size = qr.size() as usize;
        let mut dark = Vec::with_capacity(size * size);
        for y in 0..size {
            for x in 0..size {
                dark.push(qr.get_module(x as i32, y as i32));
            }
        }
        Ok(QrGrid { size, dark })
    }

    /// Modules per side, without the quiet zone.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Modules per side with the quiet zone.
    pub fn total(&self) -> usize {
        self.size + 2 * QR_QUIET
    }

    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        self.dark[y * self.size + x]
    }
}

/// Physical pixels per module: a whole number, large enough that `total` modules span at least
/// `min_points` points at `pixels_per_point`.
pub fn module_px(total: usize, pixels_per_point: f32, min_points: f32) -> u32 {
    let ppp = pixels_per_point.max(0.1);
    ((min_points * ppp) / total.max(1) as f32).ceil().max(1.0) as u32
}

/// Side of the painted code, in points, at `pixels_per_point`.
pub fn qr_side(grid: &QrGrid, pixels_per_point: f32) -> f32 {
    let ppp = pixels_per_point.max(0.1);
    module_px(grid.total(), ppp, QR_MIN_POINTS) as f32 * grid.total() as f32 / ppp
}

/// Paints the code: dark modules on a white square (whatever the theme), each module a whole
/// number of physical pixels, aligned to the pixel grid so the edges stay crisp.
pub fn paint_qr(ui: &mut Ui, grid: &QrGrid) -> Response {
    let ppp = ui.ctx().pixels_per_point();
    let total = grid.total();
    let px = module_px(total, ppp, QR_MIN_POINTS);
    let unit = px as f32 / ppp;
    let side = unit * total as f32;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    let snap = |v: f32| (v * ppp).round() / ppp;
    let origin = pos2(snap(rect.min.x), snap(rect.min.y));
    let p = ui.painter();
    p.rect_filled(
        Rect::from_min_size(origin, Vec2::splat(side)),
        0,
        Color32::WHITE,
    );
    for y in 0..grid.size() {
        let mut x = 0;
        while x < grid.size() {
            if !grid.is_dark(x, y) {
                x += 1;
                continue;
            }
            let start = x;
            while x < grid.size() && grid.is_dark(x, y) {
                x += 1;
            }
            let min = origin + vec2((QR_QUIET + start) as f32, (QR_QUIET + y) as f32) * unit;
            p.rect_filled(
                Rect::from_min_size(min, vec2((x - start) as f32 * unit, unit)),
                0,
                Color32::BLACK,
            );
        }
    }
    resp
}

// ------------------------------------------------------------------ card

fn mono_value(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).monospace().color(pal.text_strong)).truncate())
        .on_hover_text(text);
}

fn note(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).small().color(pal.weak)).wrap());
}

/// The Mobile access card's relay, fingerprint, devices and pairing parts (below the lifetime
/// controls).
pub fn show(ui: &mut Ui, input: &MobileInput<'_>, actions: &mut Vec<MobileAction>) {
    let pal = Palette::of_ui(ui);
    let Some(view) = input.view else {
        ui.add_space(6.0);
        note(
            ui,
            pal,
            "Relay, devices and pairing appear once the gateway reports.",
        );
        return;
    };
    ui.add_space(8.0);
    card::section(ui, "Relay");
    ui.add_space(4.0);
    match &view.relay {
        None => {
            note(
                ui,
                pal,
                "A relay carries the encrypted link between this computer and your phone. \
                 Self-host omp-deck-relay and enter its address and room key here.",
            );
            ui.add_space(6.0);
            if DraftButton::new("Set relay…")
                .kind(DraftKind::Primary)
                .icon(ICON_GLOBE)
                .enabled(input.gateway_up)
                .show(ui)
                .clicked()
            {
                actions.push(MobileAction::EditRelay);
            }
        }
        Some(relay) => {
            card::kv(ui, KEY_W, "URL", |ui| mono_value(ui, pal, &relay.url));
            card::kv(ui, KEY_W, "Room", |ui| mono_value(ui, pal, &relay.room_id));
            let label = link_label(&view.link, input.mode_off);
            card::kv(ui, KEY_W, "Link", |ui| {
                badge::show(ui, None, &label.text, label.hue, 11.0);
            });
            if let Some(d) = &label.detail {
                ui.add_space(2.0);
                ui.add(egui::Label::new(RichText::new(d).small().color(pal.weak)).wrap());
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if can_reconnect(Some(view), input.mode_off)
                    && DraftButton::new("Reconnect")
                        .kind(DraftKind::Primary)
                        .icon(ICON_REFRESH_CW)
                        .enabled(input.gateway_up)
                        .show(ui)
                        .on_hover_text(
                            "Take the relay room back from the other gateway / connect now",
                        )
                        .clicked()
                {
                    actions.push(MobileAction::Reconnect);
                }
                if DraftButton::new("Change…")
                    .icon(ICON_PENCIL)
                    .enabled(input.gateway_up)
                    .show(ui)
                    .clicked()
                {
                    actions.push(MobileAction::EditRelay);
                }
                if DraftButton::new("Clear")
                    .kind(DraftKind::Danger)
                    .icon(ICON_X)
                    .enabled(input.gateway_up)
                    .show(ui)
                    .clicked()
                {
                    actions.push(MobileAction::ClearRelay);
                }
            });
        }
    }

    ui.add_space(12.0);
    card::section(ui, "This desktop");
    ui.add_space(4.0);
    if view.fingerprint.is_empty() {
        note(ui, pal, "This desktop's key is not available yet.");
    } else {
        card::kv(ui, KEY_W, "Fingerprint", |ui| {
            mono_value(ui, pal, &view.fingerprint);
        });
        note(
            ui,
            pal,
            "Phones show this same fingerprint while pairing. If it differs, do not approve.",
        );
    }

    ui.add_space(12.0);
    card::section(ui, "Paired devices");
    ui.add_space(4.0);
    if view.devices.is_empty() {
        note(
            ui,
            pal,
            "No devices paired yet. Use “Pair a device” and scan the code with your phone.",
        );
    }
    for d in &view.devices {
        device_row(ui, pal, d, input.now_ms, actions);
        ui.add_space(6.0);
    }
    ui.add_space(4.0);
    let reason = pair_block(input.view, input.mode_off, input.gateway_up);
    let resp = DraftButton::new("Pair a device")
        .kind(DraftKind::Primary)
        .icon(ICON_PLUS)
        .enabled(reason.is_none())
        .show(ui);
    if resp.clicked() {
        actions.push(MobileAction::Pair);
    }
    if let Some(r) = reason {
        resp.on_hover_text(r.text());
    }
}

/// The permission badges of a device: View is always there.
pub fn permission_badges(ui: &mut Ui, p: Permissions) {
    badge::show(ui, None, "View", Hue::Blue, 10.5);
    if p.control {
        badge::show(ui, None, "Control", Hue::Green, 10.5);
    }
    if p.manage {
        badge::show(ui, None, "Manage", Hue::Orange, 10.5);
    }
}

/// One device: name, platform, permission badges, fingerprint and presence. Hover (or the
/// right-click menu) offers Rename, Permissions and Revoke.
fn device_row(
    ui: &mut Ui,
    pal: &Palette,
    d: &DeviceView,
    now_ms: u64,
    actions: &mut Vec<MobileAction>,
) {
    const PAD_X: f32 = 12.0;
    const PAD_Y: f32 = 8.0;
    let body = ui.text_style_height(&egui::TextStyle::Body);
    let small = ui.text_style_height(&egui::TextStyle::Small);
    let btn = theme::dens(28.0, 32.0);
    let h = PAD_Y * 2.0 + body.max(btn) + 24.0 + small + 8.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    let hovered = ui.rect_contains_pointer(rect) || resp.context_menu_opened();
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), hovered, 0.1);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let r = theme::ui_rad(8);
        p.rect_filled(
            rect,
            r,
            theme::mix(pal.bg, theme::hover_fill(pal), 0.35 + 0.65 * hov),
        );
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0, theme::mix(pal.stroke, pal.hover_line, hov)),
            StrokeKind::Inside,
        );
    }
    let inner = rect.shrink2(vec2(PAD_X, PAD_Y));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    let ui = &mut child;
    egui::Sides::new().shrink_left().show(
        ui,
        |ui| {
            let (dot, _) = ui.allocate_exact_size(vec2(10.0, 14.0), Sense::hover());
            if d.online {
                theme::glow_dot(ui.painter(), dot.center(), 3.5, pal.ok, theme::GLOW_REST);
            } else {
                ui.painter()
                    .circle_filled(dot.center(), 3.0, pal.weak.gamma_multiply(0.5));
            }
            ui.add(
                egui::Label::new(RichText::new(&d.name).strong().color(pal.text_strong)).truncate(),
            )
            .on_hover_text(&d.name);
            ui.add(egui::Label::new(RichText::new(&d.platform).small().color(pal.weak)).truncate());
        },
        |ui| {
            if hovered {
                if icon_button(ui, ICON_TRASH_2, "Revoke this device", false).clicked() {
                    actions.push(MobileAction::Revoke(d.id.clone()));
                }
                if icon_button(ui, ICON_KEY_ROUND, "Permissions", false).clicked() {
                    actions.push(MobileAction::Permissions(d.id.clone()));
                }
                if icon_button(ui, ICON_PENCIL, "Rename", false).clicked() {
                    actions.push(MobileAction::Rename(d.id.clone()));
                }
            }
        },
    );
    ui.horizontal(|ui| permission_badges(ui, d.permissions));
    ui.horizontal(|ui| {
        ui.label(RichText::new("FINGERPRINT").size(10.0).color(pal.glyph));
        ui.add(
            egui::Label::new(
                RichText::new(&d.fingerprint)
                    .monospace()
                    .small()
                    .color(pal.text),
            )
            .truncate(),
        )
        .on_hover_text(&d.fingerprint);
        let color = if d.online { pal.ok } else { pal.weak };
        ui.label(RichText::new(presence_text(d, now_ms)).small().color(color));
    });
    device_menu(&resp, d, actions);
}

fn device_menu(resp: &Response, d: &DeviceView, actions: &mut Vec<MobileAction>) {
    theme::context_menu(resp, 190.0, |ui| {
        let items: [(Icon, &str, bool, MobileAction); 3] = [
            (
                ICON_PENCIL,
                "Rename…",
                false,
                MobileAction::Rename(d.id.clone()),
            ),
            (
                ICON_KEY_ROUND,
                "Permissions…",
                false,
                MobileAction::Permissions(d.id.clone()),
            ),
            (
                ICON_TRASH_2,
                "Revoke…",
                true,
                MobileAction::Revoke(d.id.clone()),
            ),
        ];
        for (icon, label, danger, action) in items {
            if menu_item(ui, icon, label, None, true, danger).clicked() {
                actions.push(action);
                ui.close();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_service::gateway::protocol::RelayView;

    fn device(online: bool, last_seen_ms: u64) -> DeviceView {
        DeviceView {
            id: "d1".into(),
            name: "Pixel".into(),
            platform: "Android".into(),
            fingerprint: "aaaa bbbb".into(),
            permissions: Permissions::default(),
            created_unix_ms: 1,
            last_seen_unix_ms: last_seen_ms,
            online,
        }
    }

    fn view(link: RelayLink) -> MobileView {
        MobileView {
            relay: Some(RelayView {
                url: "wss://r.example".into(),
                room_id: "room".into(),
            }),
            link,
            fingerprint: "ffff".into(),
            devices: Vec::new(),
            pairing: None,
        }
    }

    #[test]
    fn link_labels() {
        let off = link_label(&RelayLink::Off, true);
        assert_eq!(off.text, "Off");
        assert_eq!(
            off.detail.as_deref(),
            Some("Turn on Mobile access to connect")
        );
        assert_eq!(link_label(&RelayLink::Off, false).detail, None);
        let c = link_label(
            &RelayLink::Connecting {
                attempt: 3,
                error: Some("dns".into()),
            },
            false,
        );
        assert_eq!(c.text, "Connecting, attempt 3");
        assert_eq!(c.detail.as_deref(), Some("dns"));
        assert_eq!(link_label(&RelayLink::Up, false).text, "Connected");
        let r = link_label(
            &RelayLink::Refused {
                reason: "bad key".into(),
            },
            false,
        );
        assert_eq!(
            (r.text.as_str(), r.detail.as_deref()),
            ("Refused", Some("bad key"))
        );
    }

    #[test]
    fn reconnect_only_when_refused_or_backing_off() {
        let connecting = |attempt| RelayLink::Connecting {
            attempt,
            error: None,
        };
        let refused = RelayLink::Refused { reason: "x".into() };
        assert!(can_reconnect(Some(&view(refused.clone())), false));
        assert!(can_reconnect(Some(&view(connecting(2))), false));
        assert!(!can_reconnect(Some(&view(connecting(0))), false));
        assert!(!can_reconnect(Some(&view(RelayLink::Up)), false));
        assert!(!can_reconnect(Some(&view(RelayLink::Off)), false));
        assert!(!can_reconnect(Some(&view(refused.clone())), true));
        assert!(!can_reconnect(None, false));
        let mut v = view(refused);
        v.relay = None;
        assert!(!can_reconnect(Some(&v), false));
    }

    #[test]
    fn pairing_needs_the_link_up() {
        assert_eq!(pair_block(Some(&view(RelayLink::Up)), false, true), None);
        assert_eq!(
            pair_block(Some(&view(RelayLink::Up)), false, false),
            Some(PairBlock::GatewayDown)
        );
        assert_eq!(pair_block(None, false, true), Some(PairBlock::NoReport));
        let mut v = view(RelayLink::Up);
        v.relay = None;
        assert_eq!(pair_block(Some(&v), false, true), Some(PairBlock::NoRelay));
        assert_eq!(
            pair_block(Some(&view(RelayLink::Off)), true, true),
            Some(PairBlock::ModeOff)
        );
        assert_eq!(
            pair_block(Some(&view(RelayLink::Off)), false, true),
            Some(PairBlock::NotRunning)
        );
        let refused = view(RelayLink::Refused {
            reason: "nope".into(),
        });
        let block = pair_block(Some(&refused), false, true).unwrap();
        assert!(block.text().contains("nope"));
        assert!(!block.short().contains("nope"));
        let mut open = view(RelayLink::Up);
        open.pairing = Some(deck_service::gateway::protocol::PairingView {
            pairing: "p".into(),
            expires_unix_ms: 1,
            state: deck_service::gateway::protocol::PairingState::Waiting,
        });
        assert_eq!(
            pair_block(Some(&open), false, true),
            Some(PairBlock::AlreadyOpen)
        );
    }

    #[test]
    fn last_seen() {
        let now = 10_000_000_000;
        assert_eq!(presence_text(&device(true, 0), now), "online");
        assert_eq!(presence_text(&device(false, 0), now), "never connected");
        assert_eq!(
            presence_text(&device(false, now - 5_000), now),
            "last seen just now"
        );
        assert_eq!(
            presence_text(&device(false, now - 5 * 60_000), now),
            "last seen 5m ago"
        );
        assert_eq!(
            presence_text(&device(false, now - 3 * 86_400_000), now),
            "last seen 3d ago"
        );
    }

    #[test]
    fn countdown_rounds_up_and_floors_at_zero() {
        assert_eq!(countdown(120_000, 0), "2:00");
        assert_eq!(countdown(120_000, 500), "2:00");
        assert_eq!(countdown(120_000, 1_000), "1:59");
        assert_eq!(countdown(1_000, 5_000), "0:00");
    }

    #[test]
    fn qr_grid_matches_the_library_and_sizes_crisply() {
        let grid = QrGrid::encode("https://relay.example.com/#pair=v1.abcdef").unwrap();
        let qr = qrcodegen::QrCode::encode_text(
            "https://relay.example.com/#pair=v1.abcdef",
            qrcodegen::QrCodeEcc::Medium,
        )
        .unwrap();
        assert_eq!(grid.size() as i32, qr.size());
        assert_eq!(grid.total(), grid.size() + 8);
        for y in 0..grid.size() {
            for x in 0..grid.size() {
                assert_eq!(grid.is_dark(x, y), qr.get_module(x as i32, y as i32));
            }
        }
        // Finder pattern corner is dark.
        assert!(grid.is_dark(0, 0));
        for total in [29, 49, 61, 100] {
            for ppp in [1.0f32, 1.25, 1.5, 2.0] {
                let px = module_px(total, ppp, QR_MIN_POINTS);
                assert!(px >= 1);
                assert!(
                    px as f32 * total as f32 / ppp >= QR_MIN_POINTS,
                    "{total} {ppp}"
                );
                assert!(((px - 1) as f32 * total as f32 / ppp) < QR_MIN_POINTS || px == 1);
            }
        }
    }

    #[test]
    fn qr_rejects_oversized_text() {
        assert!(QrGrid::encode(&"x".repeat(5000)).is_err());
    }

    #[test]
    fn pairing_endings() {
        let waiting = PairingSeen {
            id: "p".into(),
            expires_ms: 1_000,
            approval: None,
        };
        let asked = PairingSeen {
            approval: Some(("Pixel".into(), "aaaa bbbb".into())),
            ..waiting.clone()
        };
        let paired = [device(false, 0)];
        assert_eq!(
            pairing_end(&asked, None, &paired, 500),
            PairingEnd::Paired("Pixel".into())
        );
        assert_eq!(
            pairing_end(&asked, Some(LocalEnd::Approved), &[], 500),
            PairingEnd::Paired("Pixel".into())
        );
        assert_eq!(
            pairing_end(&asked, Some(LocalEnd::Denied), &[], 500),
            PairingEnd::Denied
        );
        assert_eq!(
            pairing_end(&waiting, Some(LocalEnd::Cancelled), &[], 500),
            PairingEnd::Cancelled
        );
        assert_eq!(pairing_end(&waiting, None, &[], 2_000), PairingEnd::Expired);
        assert_eq!(pairing_end(&waiting, None, &[], 500), PairingEnd::Closed);
        // A device that merely shares nothing with the pairing does not count as paired.
        assert_eq!(pairing_end(&asked, None, &[], 500), PairingEnd::Closed);
    }
}
