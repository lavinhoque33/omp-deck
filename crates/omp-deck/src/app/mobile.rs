//! App-side glue for mobile access: the relay form, device dialogs, and the pairing dialog every
//! window shows while the gateway has a pairing open. The gateway decides everything; this only
//! asks (typed mutations on [`crate::gw::Gw`]) and shows what `Topic::Mobile` says.
//!
//! Secrets: the room key lives in the form's text field until Save (then it moves into a
//! [`SecretText`]); the pairing link lives in [`Started`] only while the dialog needs it. Both
//! are wiped when dropped.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deck_core::config::GatewayMode;
use deck_service::gateway::protocol::{
    DeviceView, MobileView, PairingState, PairingView, Permissions, SecretText,
};
use egui::{Context, Id, Label, Margin, RichText, TextEdit};
use zeroize::Zeroize;

use super::App;
use crate::badge::{self, Hue};
use crate::card;
use crate::confirm::{self, Confirm, Dialog, show_dialog, show_dialog_labeled};
use crate::gw::PairingStarted;
use crate::icons::{ICON_GLOBE, ICON_KEY_ROUND, ICON_LOCK, ICON_PENCIL};
use crate::mobile_view::{
    LocalEnd, MobileAction, PairingEnd, PairingSeen, QrGrid, can_reconnect, countdown, paint_qr,
    pair_block, pairing_end, permission_badges, qr_side,
};
use crate::theme::Palette;
use crate::widgets::{setting_row, toggle};

/// Key column of the dialogs' key/value rows.
const KEY_W: f32 = 92.0;

/// Permission explanations shared by the approval dialog and the device editor. Control is
/// arbitrary code execution as the desktop user (omp's `!` shell, terminal typing); say so.
const VIEW_TEXT: &str =
    "Always allowed: every session's transcript, terminal output and uncommitted changes.";
const CONTROL_TEXT: &str = "Full remote control of this computer as your account: prompts and \
                            terminal typing can run any command. Allow only on a phone you trust \
                            as much as this computer.";
const MANAGE_TEXT: &str = "Also delete, copy and move sessions and discard git changes without \
                           the terminal (Control can already do this through prompts).";

/// Trims `s` where it is: its contents are never copied, so nothing is left behind in freed
/// memory (the buffer is wiped when the resulting [`SecretText`] drops).
fn trim_in_place(mut s: String) -> String {
    s.truncate(s.trim_end().len());
    let lead = s.len() - s.trim_start().len();
    s.drain(..lead);
    s
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

struct RelayForm {
    url: String,
    /// The typed room key; moved into a [`SecretText`] on Save and wiped on drop.
    key: String,
    error: Option<String>,
    busy: bool,
    focus: bool,
    /// A relay is configured already ("Change relay").
    change: bool,
}

impl Drop for RelayForm {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

struct RenameForm {
    id: String,
    name: String,
    original: String,
    error: Option<String>,
    busy: bool,
    focus: bool,
}

struct PermForm {
    id: String,
    name: String,
    perms: Permissions,
    original: Permissions,
    error: Option<String>,
    busy: bool,
}

/// A pairing this window started: the link behind the QR code (a secret) and the code itself.
struct Started {
    pairing: String,
    link: SecretText,
    expires_ms: u64,
    qr: Result<QrGrid, String>,
}

pub(super) struct MobileUi {
    relay: Option<RelayForm>,
    rename: Option<RenameForm>,
    perms: Option<PermForm>,
    /// `(device id, confirmation)`.
    revoke: Option<(String, Confirm)>,
    clear: Option<Confirm>,
    started: Option<Started>,
    /// `PairingBegin` is in flight.
    beginning: bool,
    /// The approval dialog's toggles.
    approve: Permissions,
    /// The pairing the dialog last showed, to tell how it ended.
    seen: Option<PairingSeen>,
    local_end: Option<LocalEnd>,
    /// A waiting pairing from another window the user chose to hide.
    hidden: Option<String>,
    /// Approve / deny / cancel is in flight.
    deciding: bool,
}

impl MobileUi {
    pub(super) fn new() -> MobileUi {
        MobileUi {
            relay: None,
            rename: None,
            perms: None,
            revoke: None,
            clear: None,
            started: None,
            beginning: false,
            approve: Permissions::default(),
            seen: None,
            local_end: None,
            hidden: None,
            deciding: false,
        }
    }

    fn forms_open(&self) -> bool {
        self.relay.is_some()
            || self.rename.is_some()
            || self.perms.is_some()
            || self.revoke.is_some()
            || self.clear.is_some()
    }

    /// Whether the pairing dialog shows `p` in this window.
    fn shows(&self, p: &PairingView) -> bool {
        !(matches!(p.state, PairingState::Waiting) && self.hidden.as_deref() == Some(&p.pairing))
    }
}

impl App {
    /// A mobile-access dialog is open (or a pairing is on screen).
    pub(super) fn mobile_modal_open(&self) -> bool {
        self.mobile.forms_open()
            || self
                .gw
                .mobile()
                .and_then(|v| v.pairing.clone())
                .is_some_and(|p| self.mobile.shows(&p))
    }

    fn mobile_mode_off(&self) -> bool {
        self.gw.gateway().is_none_or(|g| g.mode == GatewayMode::Off)
    }

    fn mobile_device(&self, id: &str) -> Option<DeviceView> {
        self.gw
            .mobile()?
            .devices
            .iter()
            .find(|d| d.id == id)
            .cloned()
    }

    pub(super) fn apply_mobile_action(&mut self, action: MobileAction) {
        match action {
            MobileAction::EditRelay => self.open_relay_form(),
            MobileAction::Reconnect => self.reconnect_relay(),
            MobileAction::ClearRelay => {
                self.mobile.clear = Some(Confirm {
                    title: "Clear relay".to_owned(),
                    body: "Forget this relay? Mobile access stops, and paired devices stay \
                           listed but cannot connect until a relay is set again."
                        .to_owned(),
                    confirm_label: "Clear".to_owned(),
                    danger: true,
                });
            }
            MobileAction::Pair => self.start_pairing(),
            MobileAction::Rename(id) => {
                if let Some(d) = self.mobile_device(&id) {
                    self.mobile.rename = Some(RenameForm {
                        id,
                        original: d.name.clone(),
                        name: d.name,
                        error: None,
                        busy: false,
                        focus: true,
                    });
                }
            }
            MobileAction::Permissions(id) => {
                if let Some(d) = self.mobile_device(&id) {
                    self.mobile.perms = Some(PermForm {
                        id,
                        name: d.name,
                        perms: d.permissions,
                        original: d.permissions,
                        error: None,
                        busy: false,
                    });
                }
            }
            MobileAction::Revoke(id) => {
                if let Some(d) = self.mobile_device(&id) {
                    self.mobile.revoke = Some((
                        id,
                        Confirm {
                            title: "Revoke device".to_owned(),
                            body: format!(
                                "Revoke “{}”? It is disconnected at once and has to be paired \
                                 again to get back in.",
                                d.name
                            ),
                            confirm_label: "Revoke".to_owned(),
                            danger: true,
                        },
                    ));
                }
            }
        }
    }

    /// "Reconnect" (settings button, palette): restart the relay link now.
    pub(super) fn reconnect_relay(&mut self) {
        let view = self.gw.mobile();
        if !self.gw.is_up() || !can_reconnect(view.as_deref(), self.mobile_mode_off()) {
            self.toasts.info(
                &self.ctx,
                "The relay link does not need a reconnect right now.",
            );
            return;
        }
        self.gw.relay_reconnect(|app, r| match r {
            Ok(()) => app.toasts.info(&app.ctx, "Reconnecting to the relay…"),
            Err(e) => app
                .toasts
                .error(&app.ctx, format!("Could not reconnect: {e}")),
        });
    }

    fn open_relay_form(&mut self) {
        let current = self.gw.mobile().and_then(|v| v.relay.clone());
        self.mobile.relay = Some(RelayForm {
            url: current.as_ref().map_or_else(String::new, |r| r.url.clone()),
            key: String::new(),
            error: None,
            busy: false,
            focus: true,
            change: current.is_some(),
        });
    }

    /// "Pair a device" (settings button, palette): the same gate everywhere.
    pub(super) fn start_pairing(&mut self) {
        let view = self.gw.mobile();
        if let Some(block) = pair_block(view.as_deref(), self.mobile_mode_off(), self.gw.is_up()) {
            self.toasts.info(&self.ctx, block.text());
            return;
        }
        if self.mobile.beginning {
            return;
        }
        self.mobile.beginning = true;
        self.mobile.local_end = None;
        self.mobile.hidden = None;
        self.gw.pairing_begin(|app, r| {
            app.mobile.beginning = false;
            match r {
                Ok(PairingStarted {
                    pairing,
                    link,
                    expires_unix_ms,
                }) => {
                    let qr = QrGrid::encode(link.expose());
                    app.mobile.started = Some(Started {
                        pairing,
                        link,
                        expires_ms: expires_unix_ms,
                        qr,
                    });
                }
                Err(e) => app
                    .toasts
                    .error(&app.ctx, format!("Could not start pairing: {e}")),
            }
        });
    }

    /// Every frame: the mobile-access dialogs.
    pub(super) fn show_mobile_dialogs(&mut self, ctx: &Context) {
        let view = self.gw.mobile();
        self.track_pairing(view.as_deref());
        if let Some(p) = view.as_deref().and_then(|v| v.pairing.as_ref()) {
            self.show_pairing_dialog(ctx, p);
        }
        self.show_relay_form(ctx);
        self.show_rename_form(ctx);
        self.show_perm_form(ctx);
        self.show_revoke(ctx);
        self.show_clear(ctx);
    }

    /// Follows the topic's pairing: remembers what it showed and, once it is gone, says how it
    /// ended and drops the link.
    fn track_pairing(&mut self, view: Option<&MobileView>) {
        let now = now_ms();
        let Some(view) = view else {
            return;
        };
        match &view.pairing {
            Some(p) => {
                let approval = match &p.state {
                    PairingState::Approval {
                        name, fingerprint, ..
                    } => Some((name.clone(), fingerprint.clone())),
                    PairingState::Waiting => None,
                };
                let fresh = match &self.mobile.seen {
                    None => true,
                    Some(s) => s.id != p.pairing || (s.approval.is_none() && approval.is_some()),
                };
                if fresh {
                    // A new pairing, or a device just asked: the proposal starts over.
                    self.mobile.approve = Permissions::default();
                    self.mobile.deciding = false;
                }
                if self.mobile.seen.as_ref().is_some_and(|s| s.id != p.pairing) {
                    self.mobile.local_end = None;
                    self.mobile.hidden = None;
                }
                if self
                    .mobile
                    .started
                    .as_ref()
                    .is_some_and(|s| s.pairing != p.pairing)
                {
                    self.mobile.started = None;
                }
                self.mobile.seen = Some(PairingSeen {
                    id: p.pairing.clone(),
                    expires_ms: p.expires_unix_ms,
                    approval,
                });
            }
            None => {
                if let Some(seen) = self.mobile.seen.take() {
                    let local = self.mobile.local_end.take();
                    let end = pairing_end(&seen, local, &view.devices, now);
                    self.mobile.started = None;
                    self.mobile.hidden = None;
                    self.mobile.deciding = false;
                    match end {
                        PairingEnd::Paired(name) => {
                            self.toasts.success(&self.ctx, format!("{name} paired"));
                        }
                        PairingEnd::Denied => self.toasts.info(&self.ctx, "Pairing denied"),
                        PairingEnd::Cancelled => self.toasts.info(&self.ctx, "Pairing cancelled"),
                        PairingEnd::Expired => self.toasts.info(&self.ctx, "Pairing expired"),
                        PairingEnd::Closed => self.toasts.info(
                            &self.ctx,
                            "Pairing ended: the device disconnected or the pairing was closed elsewhere",
                        ),
                    }
                } else if self
                    .mobile
                    .started
                    .as_ref()
                    .is_some_and(|s| now >= s.expires_ms)
                {
                    // Opened and closed between two frames: nothing to show, nothing to keep.
                    self.mobile.started = None;
                }
            }
        }
    }

    fn show_pairing_dialog(&mut self, ctx: &Context, p: &PairingView) {
        if !self.mobile.shows(p) {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let pal = Palette::of_ctx(ctx);
        let left = countdown(p.expires_unix_ms, now_ms());
        match &p.state {
            PairingState::Waiting => {
                let mine = self
                    .mobile
                    .started
                    .as_ref()
                    .filter(|s| s.pairing == p.pairing);
                if mine.is_none() && !self.mobile.beginning {
                    let answer = show_dialog_labeled(
                        ctx,
                        &Dialog {
                            id: "mobile-pairing-other",
                            title: "Pairing in progress",
                            width: 440.0,
                            confirm_label: "Cancel pairing",
                            confirm_enabled: !self.mobile.deciding,
                            icon: Some(ICON_GLOBE),
                            subtitle: Some("Started in another window"),
                            ..Dialog::default()
                        },
                        "Close",
                        |ui| {
                            card::kv(ui, KEY_W, "Expires in", |ui| {
                                badge::show(ui, None, &left, Hue::Amber, 11.0);
                            });
                            ui.add_space(6.0);
                            ui.add(
                                Label::new(
                                    RichText::new(
                                        "Another OmpDeck window opened a pairing window. The \
                                         device's request shows up here once it scans the code.",
                                    )
                                    .color(pal.weak),
                                )
                                .wrap(),
                            );
                        },
                    );
                    match answer {
                        Some(true) => self.cancel_pairing(),
                        Some(false) => self.mobile.hidden = Some(p.pairing.clone()),
                        None => {}
                    }
                    return;
                }
                let side = mine
                    .and_then(|s| s.qr.as_ref().ok())
                    .map_or(240.0, |g| qr_side(g, ctx.pixels_per_point()));
                let answer = show_dialog_labeled(
                    ctx,
                    &Dialog {
                        id: "mobile-pairing",
                        title: "Pair a device",
                        width: (side + 72.0).max(440.0),
                        confirm_label: "Copy link",
                        confirm_enabled: mine.is_some(),
                        icon: Some(ICON_GLOBE),
                        subtitle: Some("Scan with OmpDeck on your phone"),
                        ..Dialog::default()
                    },
                    "Cancel",
                    |ui| {
                        match mine.map(|s| &s.qr) {
                            Some(Ok(grid)) => {
                                ui.vertical_centered(|ui| {
                                    paint_qr(ui, grid);
                                });
                            }
                            Some(Err(e)) => {
                                ui.add(Label::new(RichText::new(e).color(pal.danger)).wrap());
                            }
                            None => {
                                ui.horizontal(|ui| {
                                    ui.add(crate::widgets::Spin::new(18.0, pal.accent_b));
                                    ui.label(RichText::new("Starting…").color(pal.weak));
                                });
                            }
                        }
                        ui.add_space(10.0);
                        card::kv(ui, KEY_W, "Expires in", |ui| {
                            badge::show(ui, None, &left, Hue::Amber, 11.0);
                        });
                        ui.add_space(6.0);
                        ui.add(
                            Label::new(
                                RichText::new(
                                    "Scan the code, or copy the link and open it on your phone. \
                                     The phone will show a fingerprint to compare before \
                                     anything is allowed.",
                                )
                                .small()
                                .color(pal.weak),
                            )
                            .wrap(),
                        );
                    },
                );
                match answer {
                    Some(true) => {
                        if let Some(s) = &self.mobile.started {
                            self.clipboard.set_text(s.link.expose());
                            self.toasts.success(&self.ctx, "Copied pairing link");
                        }
                    }
                    Some(false) => self.cancel_pairing(),
                    None => {}
                }
            }
            PairingState::Approval {
                name,
                platform,
                fingerprint,
            } => {
                let deciding = self.mobile.deciding;
                let approve = &mut self.mobile.approve;
                let answer = show_dialog_labeled(
                    ctx,
                    &Dialog {
                        id: "mobile-approval",
                        title: "Approve this device?",
                        width: 480.0,
                        confirm_label: "Approve",
                        confirm_enabled: !deciding,
                        icon: Some(ICON_KEY_ROUND),
                        subtitle: Some("Compare the fingerprint with your phone"),
                        ..Dialog::default()
                    },
                    "Deny",
                    |ui| {
                        card::kv(ui, KEY_W, "Device", |ui| {
                            ui.add(
                                Label::new(RichText::new(name).strong().color(pal.text_strong))
                                    .truncate(),
                            );
                        });
                        card::kv(ui, KEY_W, "Platform", |ui| {
                            badge::show(ui, None, platform, Hue::Teal, 11.0);
                        });
                        card::kv(ui, KEY_W, "Expires in", |ui| {
                            badge::show(ui, None, &left, Hue::Amber, 11.0);
                        });
                        ui.add_space(8.0);
                        card::section(ui, "Fingerprint");
                        ui.add_space(4.0);
                        egui::Frame::NONE
                            .fill(pal.bg)
                            .stroke(egui::Stroke::new(1.0, pal.stroke))
                            .corner_radius(crate::theme::ui_rad(8))
                            .inner_margin(Margin::symmetric(14, 12))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.vertical_centered(|ui| {
                                    ui.add(
                                        Label::new(
                                            RichText::new(fingerprint)
                                                .monospace()
                                                .size(20.0)
                                                .color(pal.text_strong),
                                        )
                                        .wrap(),
                                    );
                                });
                            });
                        ui.add_space(4.0);
                        ui.add(
                            Label::new(
                                RichText::new(
                                    "Compare with the phone: approve only if it shows exactly \
                                     this.",
                                )
                                .small()
                                .color(pal.warn),
                            )
                            .wrap(),
                        );
                        ui.add_space(8.0);
                        card::section(ui, "Permissions");
                        setting_row(ui, "View", VIEW_TEXT, |ui| {
                            badge::show(ui, None, "always", Hue::Blue, 11.0);
                        });
                        setting_row(ui, "Control", CONTROL_TEXT, |ui| {
                            toggle(ui, &mut approve.control);
                        });
                        setting_row(ui, "Manage", MANAGE_TEXT, |ui| {
                            toggle(ui, &mut approve.manage);
                        });
                    },
                );
                match answer {
                    Some(true) if !deciding => self.decide_pairing(&p.pairing, true),
                    Some(false) if !deciding => self.decide_pairing(&p.pairing, false),
                    _ => {}
                }
            }
        }
    }

    fn cancel_pairing(&mut self) {
        if self.mobile.deciding {
            return;
        }
        self.mobile.deciding = true;
        self.mobile.local_end = Some(LocalEnd::Cancelled);
        self.gw.pairing_cancel(|app, r| {
            if let Err(e) = r {
                app.mobile.deciding = false;
                app.mobile.local_end = None;
                app.toasts
                    .error(&app.ctx, format!("Could not cancel pairing: {e}"));
            }
        });
    }

    fn decide_pairing(&mut self, pairing: &str, approve: bool) {
        self.mobile.deciding = true;
        let done = move |app: &mut App, r: Result<(), String>| {
            if let Err(e) = r {
                app.mobile.deciding = false;
                app.mobile.local_end = None;
                let what = if approve { "approve" } else { "deny" };
                app.toasts
                    .error(&app.ctx, format!("Could not {what} the device: {e}"));
            }
        };
        if approve {
            self.mobile.local_end = Some(LocalEnd::Approved);
            let perms = self.mobile.approve;
            self.gw.pairing_approve(pairing.to_owned(), perms, done);
        } else {
            self.mobile.local_end = Some(LocalEnd::Denied);
            self.gw.pairing_deny(pairing.to_owned(), done);
        }
    }

    fn show_relay_form(&mut self, ctx: &Context) {
        let Some(f) = &mut self.mobile.relay else {
            return;
        };
        let pal = Palette::of_ctx(ctx);
        let title = if f.change {
            "Change relay"
        } else {
            "Set relay"
        };
        let ok = !f.busy && !f.url.trim().is_empty() && !f.key.trim().is_empty();
        let answer = show_dialog(
            ctx,
            &Dialog {
                id: "mobile-relay",
                title,
                width: 460.0,
                confirm_label: "Save",
                confirm_enabled: ok,
                enter_confirms: true,
                icon: Some(ICON_GLOBE),
                subtitle: Some("Where your phone and this computer meet"),
                ..Dialog::default()
            },
            |ui| {
                card::section(ui, "Relay address");
                ui.add_space(4.0);
                let url = ui.add(
                    TextEdit::singleline(&mut f.url)
                        .id(Id::unique("mobile-relay-url"))
                        .hint_text("wss://relay.example.com")
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8)),
                );
                if url.changed() {
                    f.error = None;
                }
                if f.focus {
                    url.request_focus();
                    f.focus = false;
                }
                ui.add_space(10.0);
                card::section(ui, "Room key");
                ui.add_space(4.0);
                card::kv(ui, KEY_W, "Storage", |ui| {
                    badge::show(ui, Some(ICON_LOCK), "write-only", Hue::Amber, 11.0);
                });
                ui.add_space(4.0);
                let key = ui.add(
                    TextEdit::singleline(&mut f.key)
                        .id(Id::unique("mobile-relay-key"))
                        .password(true)
                        .hint_text(if f.change {
                            "Paste the room key again to change the relay"
                        } else {
                            "Paste the room key"
                        })
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8)),
                );
                if key.changed() {
                    f.error = None;
                }
                ui.add_space(4.0);
                ui.add(
                    Label::new(
                        RichText::new(
                            "The key stays on this computer and is never shown again. Plain \
                             ws:// works only for a relay on this machine.",
                        )
                        .small()
                        .color(pal.weak),
                    )
                    .wrap(),
                );
                if let Some(e) = &f.error {
                    ui.add_space(6.0);
                    ui.add(Label::new(RichText::new(e).small().color(pal.danger)).wrap());
                }
            },
        );
        match answer {
            Some(true) if ok => {
                f.busy = true;
                f.error = None;
                let url = f.url.trim().to_owned();
                let key = SecretText::new(trim_in_place(std::mem::take(&mut f.key)));
                self.gw.relay_set(url, key, |app, r| match r {
                    Ok(()) => {
                        app.mobile.relay = None;
                        app.toasts.success(&app.ctx, "Relay saved");
                    }
                    Err(e) => {
                        if let Some(f) = &mut app.mobile.relay {
                            f.busy = false;
                            f.error = Some(e);
                        }
                    }
                });
            }
            Some(false) => self.mobile.relay = None,
            _ => {}
        }
    }

    fn show_rename_form(&mut self, ctx: &Context) {
        let Some(f) = &mut self.mobile.rename else {
            return;
        };
        let pal = Palette::of_ctx(ctx);
        let name = f.name.trim().to_owned();
        let ok = !f.busy && !name.is_empty() && name != f.original;
        let answer = show_dialog(
            ctx,
            &Dialog {
                id: "mobile-rename",
                title: "Rename device",
                width: 420.0,
                confirm_label: "Save",
                confirm_enabled: ok,
                enter_confirms: true,
                icon: Some(ICON_PENCIL),
                ..Dialog::default()
            },
            |ui| {
                card::kv(ui, KEY_W, "Device", |ui| {
                    ui.label(RichText::new(&f.original).color(pal.text_strong));
                });
                ui.add_space(8.0);
                let edit = ui.add(
                    TextEdit::singleline(&mut f.name)
                        .id(Id::unique("mobile-rename-name"))
                        .hint_text("Device name")
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(10, 8)),
                );
                if edit.changed() {
                    f.error = None;
                }
                if f.focus {
                    edit.request_focus();
                    f.focus = false;
                }
                if let Some(e) = &f.error {
                    ui.add_space(6.0);
                    ui.add(Label::new(RichText::new(e).small().color(pal.danger)).wrap());
                }
            },
        );
        match answer {
            Some(true) if ok => {
                f.busy = true;
                let id = f.id.clone();
                self.gw.device_rename(id, name, |app, r| match r {
                    Ok(()) => {
                        app.mobile.rename = None;
                        app.toasts.success(&app.ctx, "Device renamed");
                    }
                    Err(e) => {
                        if let Some(f) = &mut app.mobile.rename {
                            f.busy = false;
                            f.error = Some(e);
                        }
                    }
                });
            }
            Some(false) => self.mobile.rename = None,
            _ => {}
        }
    }

    fn show_perm_form(&mut self, ctx: &Context) {
        let Some(f) = &mut self.mobile.perms else {
            return;
        };
        let pal = Palette::of_ctx(ctx);
        let ok = !f.busy && f.perms != f.original;
        let answer = show_dialog(
            ctx,
            &Dialog {
                id: "mobile-permissions",
                title: "Device permissions",
                width: 460.0,
                confirm_label: "Save",
                confirm_enabled: ok,
                enter_confirms: true,
                icon: Some(ICON_KEY_ROUND),
                ..Dialog::default()
            },
            |ui| {
                card::kv(ui, KEY_W, "Device", |ui| {
                    ui.label(RichText::new(&f.name).strong().color(pal.text_strong));
                });
                card::kv(ui, KEY_W, "Allowed", |ui| permission_badges(ui, f.perms));
                ui.add_space(6.0);
                setting_row(ui, "View", VIEW_TEXT, |ui| {
                    badge::show(ui, None, "always", Hue::Blue, 11.0);
                });
                setting_row(ui, "Control", CONTROL_TEXT, |ui| {
                    if toggle(ui, &mut f.perms.control).changed() {
                        f.error = None;
                    }
                });
                setting_row(ui, "Manage", MANAGE_TEXT, |ui| {
                    if toggle(ui, &mut f.perms.manage).changed() {
                        f.error = None;
                    }
                });
                if let Some(e) = &f.error {
                    ui.add_space(6.0);
                    ui.add(Label::new(RichText::new(e).small().color(pal.danger)).wrap());
                }
            },
        );
        match answer {
            Some(true) if ok => {
                f.busy = true;
                let (id, perms) = (f.id.clone(), f.perms);
                self.gw.device_set_permissions(id, perms, |app, r| match r {
                    Ok(()) => {
                        app.mobile.perms = None;
                        app.toasts.success(&app.ctx, "Permissions updated");
                    }
                    Err(e) => {
                        if let Some(f) = &mut app.mobile.perms {
                            f.busy = false;
                            f.error = Some(e);
                        }
                    }
                });
            }
            Some(false) => self.mobile.perms = None,
            _ => {}
        }
    }

    fn show_revoke(&mut self, ctx: &Context) {
        let Some((id, ask)) = &self.mobile.revoke else {
            return;
        };
        match confirm::show(ctx, "mobile-revoke", ask) {
            Some(true) => {
                let id = id.clone();
                self.mobile.revoke = None;
                self.gw.device_revoke(id, |app, r| match r {
                    Ok(()) => app.toasts.success(&app.ctx, "Device revoked"),
                    Err(e) => app
                        .toasts
                        .error(&app.ctx, format!("Could not revoke the device: {e}")),
                });
            }
            Some(false) => self.mobile.revoke = None,
            None => {}
        }
    }

    fn show_clear(&mut self, ctx: &Context) {
        let Some(ask) = &self.mobile.clear else {
            return;
        };
        match confirm::show(ctx, "mobile-relay-clear", ask) {
            Some(true) => {
                self.mobile.clear = None;
                self.gw.relay_clear(|app, r| match r {
                    Ok(()) => app.toasts.success(&app.ctx, "Relay cleared"),
                    Err(e) => app
                        .toasts
                        .error(&app.ctx, format!("Could not clear the relay: {e}")),
                });
            }
            Some(false) => self.mobile.clear = None,
            None => {}
        }
    }
}
