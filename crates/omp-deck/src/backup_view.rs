//! The Settings → Backup card (destination, Google account or backup server, schedule, backup
//! and restore), the restore picker and the "Manage settings backups" dialog (pure UI: state in,
//! actions out; the logic lives in `app/backup.rs` and `deck_core::backup`).

use std::collections::HashSet;
use std::path::PathBuf;

use deck_core::backup::Destination;
use deck_core::backup::server::{DEFAULT_FOLDER, Protocol, ServerAuth, ServerConfig};
use deck_core::config::SyncInterval;
use deck_service::gateway::protocol::ServerView;
use egui::{Button, Context, RichText, ScrollArea, TextEdit, Ui, vec2};

use crate::badge::{self, Hue};
use crate::card;
use crate::confirm::{Dialog, show_dialog, show_dialog_labeled};
use crate::icons::{
    ICON_ARCHIVE_RESTORE, ICON_CLOUD, ICON_CLOUD_UPLOAD, ICON_FOLDER_OPEN, ICON_HARD_DRIVE,
    ICON_KEY_ROUND, ICON_LIST_CHECKS, ICON_LOCK, ICON_LOG_IN, ICON_LOG_OUT, ICON_PENCIL,
    ICON_REFRESH_CW, ICON_TRASH_2, ICON_TRIANGLE_ALERT, ICON_X,
};
use crate::theme::{self, Palette};
use crate::widgets::{DraftButton, DraftKind, Spin, icon_text, segmented, setting_row, toggle};

/// The Google account part of the card.
#[derive(Clone, Debug, PartialEq)]
pub enum DriveAccount {
    /// This build has no Google OAuth client.
    Unavailable,
    SignedOut,
    /// The consent page is open; the loopback waits for the browser.
    Waiting,
    SignedIn {
        email: Option<String>,
    },
}

/// What the card (and the Sync page's account strip) shows.
#[derive(Clone, Debug, PartialEq)]
pub struct BackupCard {
    pub destination: Destination,
    pub drive: DriveAccount,
    /// The saved backup server.
    pub server: Option<ServerView>,
    /// The active destination's name: "Google Drive", `sftp://host`, "server".
    pub place: String,
    /// "2026-10-22 13:52:01 UTC from host", `None` = no backup from this computer yet.
    pub last: Option<String>,
    pub interval: SyncInterval,
    pub busy: Option<Busy>,
    /// A native file dialog exists (the key file's Browse button).
    pub picker: bool,
}

impl BackupCard {
    /// The active destination is set up (signed in / a server saved).
    pub fn linked(&self) -> bool {
        match self.destination {
            Destination::Drive => matches!(self.drive, DriveAccount::SignedIn { .. }),
            Destination::Server => self.server.is_some(),
        }
    }

    /// Why backups cannot run yet (disabled buttons and menu entries).
    pub fn unlinked_reason(&self) -> &'static str {
        unlinked_reason(self.destination, self.drive == DriveAccount::Unavailable)
    }
}

/// Why the active destination cannot take backups yet.
pub fn unlinked_reason(destination: Destination, drive_unavailable: bool) -> &'static str {
    match destination {
        Destination::Drive if drive_unavailable => {
            "Google Drive is not available in this build; set up a server in Settings → Backup"
        }
        Destination::Drive => "Sign in to Google Drive in Settings → Backup first",
        Destination::Server => "No backup server is set up; add one in Settings → Backup",
    }
}

/// The running backup job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    BackingUp,
    Listing,
    Preparing,
    Restoring,
    SigningOut,
    /// Deleting backups chosen in the manager or the restore picker.
    Deleting,
    /// Checking and saving the server.
    Connecting,
    RemovingServer,
}

#[derive(Debug, PartialEq)]
pub enum BackupAction {
    SetDestination(Destination),
    SignIn,
    CancelSignIn,
    SetInterval(SyncInterval),
    BackupNow,
    OpenRestore,
    /// "Manage backups…": the dialog listing every config backup for deletion.
    OpenManager,
    SignOut,
    /// Check and save the server form's settings (`pinned` set once the user trusted it).
    Connect {
        config: Box<ServerConfig>,
        keep_secret: bool,
    },
    /// Pick the SFTP key file with the native dialog.
    BrowseKey,
    /// Forget the saved server (asks first).
    RemoveServer,
    /// Open Settings → Backup (from the Sync page).
    OpenBackupSettings,
}

/// The server form's text, kept between frames (Settings window state).
#[derive(Clone, Debug, PartialEq)]
pub struct ServerForm {
    /// Editing a saved server (else its summary shows).
    pub editing: bool,
    pub protocol: Protocol,
    pub host: String,
    /// Empty = the protocol's default port.
    pub port: String,
    /// FTP/FTPS `anonymous` login.
    pub anonymous: bool,
    pub user: String,
    pub password: String,
    /// SFTP: sign in with a key file instead of the password.
    pub use_key: bool,
    pub key_path: String,
    pub passphrase: String,
    pub folder: String,
    /// Editing a saved server: an empty password / passphrase keeps the saved one.
    pub keep_secret: bool,
    /// The last connect failure.
    pub error: Option<String>,
    /// The fingerprint the user is asked to trust.
    pub trust: Option<String>,
}

impl Default for ServerForm {
    fn default() -> ServerForm {
        ServerForm {
            editing: false,
            protocol: Protocol::Sftp,
            host: String::new(),
            port: Protocol::Sftp.default_port().to_string(),
            anonymous: false,
            user: String::new(),
            password: String::new(),
            use_key: false,
            key_path: String::new(),
            passphrase: String::new(),
            folder: DEFAULT_FOLDER.to_owned(),
            keep_secret: false,
            error: None,
            trust: None,
        }
    }
}

impl ServerForm {
    /// The form filled from the saved server, for editing (its secrets stay in the gateway).
    pub fn edit(server: &ServerView) -> ServerForm {
        ServerForm {
            editing: true,
            protocol: server.protocol,
            host: server.host.clone(),
            port: server.port.to_string(),
            anonymous: server.user.is_none(),
            user: server.user.clone().unwrap_or_default(),
            use_key: server.key_path.is_some(),
            key_path: server
                .key_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            folder: server.folder.clone(),
            keep_secret: true,
            ..ServerForm::default()
        }
    }

    /// A protocol switch: the port follows unless the user typed another one.
    pub fn set_protocol(&mut self, protocol: Protocol) {
        let port = self.port.trim();
        if port.is_empty() || port == self.protocol.default_port().to_string() {
            self.port = protocol.default_port().to_string();
        }
        self.protocol = protocol;
    }

    /// The settings to connect with; `Err` = a field that cannot work.
    pub fn config(&self, pinned: Option<String>) -> Result<ServerConfig, String> {
        let port = match self.port.trim() {
            "" => 0,
            p => match p.parse::<u16>() {
                Ok(n) if n > 0 => n,
                _ => return Err("The port must be a number from 1 to 65535".to_owned()),
            },
        };
        let sftp = self.protocol == Protocol::Sftp;
        let auth = if self.anonymous && !sftp {
            ServerAuth::Anonymous
        } else if sftp && self.use_key {
            if self.key_path.trim().is_empty() {
                return Err("Choose the private key file".to_owned());
            }
            ServerAuth::Key {
                user: self.user.clone(),
                key_path: PathBuf::from(self.key_path.trim()),
                passphrase: (!self.passphrase.is_empty()).then(|| self.passphrase.clone()),
            }
        } else {
            if self.password.is_empty() && !self.keep_secret {
                return Err("Enter the password".to_owned());
            }
            ServerAuth::Password {
                user: self.user.clone(),
                password: self.password.clone(),
            }
        };
        Ok(ServerConfig {
            protocol: self.protocol,
            host: self.host.clone(),
            port,
            auth,
            folder: self.folder.clone(),
            pinned,
        })
    }
}

fn button(
    ui: &mut Ui,
    enabled: bool,
    kind: DraftKind,
    icon: crate::icons::Icon,
    label: &str,
) -> egui::Response {
    ui.add_enabled(
        enabled,
        DraftButton::new(label)
            .kind(kind)
            .icon(icon)
            .enabled(enabled),
    )
}

/// Width of the server form's text fields (leaves the row descriptions room on one line).
const FIELD_W: f32 = 250.0;
const MANAGE_SUB: &str = "Delete old versions or another PC's backups (the newest 30 are kept)";
/// The plain-FTP warning (form and summary).
const FTP_WARNING: &str = "Plain FTP sends your login and your backups unencrypted; anyone on the \
     network path can read them. Use SFTP or FTPS if the server offers it.";

/// A wrapped explanation under a row.
fn note(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add(
        egui::Label::new(RichText::new(text).small().color(pal.weak))
            .wrap_mode(egui::TextWrapMode::Wrap),
    );
}

/// A warning line with its glyph (plain FTP, a failed connect).
fn warning(ui: &mut Ui, text: &str, color: egui::Color32) {
    ui.horizontal_top(|ui| {
        ui.label(icon_text(ICON_TRIANGLE_ALERT, 15.0, color));
        ui.add(
            egui::Label::new(RichText::new(text).color(color)).wrap_mode(egui::TextWrapMode::Wrap),
        );
    });
}

fn destination_label(d: Destination) -> &'static str {
    match d {
        Destination::Drive => "Google Drive",
        Destination::Server => "FTP · SFTP server",
    }
}

/// The card body (inside `widgets::card`).
pub fn card(ui: &mut Ui, state: &BackupCard, form: &mut ServerForm) -> Vec<BackupAction> {
    let mut actions = Vec::new();
    let idle = state.busy.is_none();
    setting_row(
        ui,
        "Back up to",
        "Config backups and project sync use one destination at a time",
        |ui| {
            ui.add_enabled_ui(idle, |ui| {
                egui::ComboBox::from_id_salt("backup-destination")
                    .selected_text(destination_label(state.destination))
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for d in [Destination::Drive, Destination::Server] {
                            let usable =
                                d != Destination::Drive || state.drive != DriveAccount::Unavailable;
                            let resp = ui
                                .add_enabled(
                                    usable,
                                    Button::selectable(
                                        state.destination == d,
                                        destination_label(d),
                                    ),
                                )
                                .on_disabled_hover_text(
                                    "This OmpDeck was built without a Google OAuth client",
                                );
                            if resp.clicked() && state.destination != d {
                                actions.push(BackupAction::SetDestination(d));
                            }
                        }
                    });
            })
            .response
            .on_disabled_hover_text("Wait for the running backup job to finish");
        },
    );
    ui.add_space(4.0);
    match state.destination {
        Destination::Drive => drive_rows(ui, state, &mut actions),
        Destination::Server => match &state.server {
            Some(server) if !form.editing => server_summary(ui, state, server, form, &mut actions),
            _ => server_form(ui, state, form, &mut actions),
        },
    }
    if state.linked() {
        backup_rows(ui, state, &mut actions);
    }
    trust_prompt(ui.ctx(), state, form, &mut actions);
    actions
}

fn drive_rows(ui: &mut Ui, state: &BackupCard, actions: &mut Vec<BackupAction>) {
    let pal = Palette::of_ui(ui);
    match &state.drive {
        DriveAccount::Unavailable => {
            setting_row(
                ui,
                "Not available in this build",
                "This OmpDeck was built without a Google OAuth client",
                |_| {},
            );
            note(
                ui,
                "Set OMP_DECK_GOOGLE_CLIENT_ID and OMP_DECK_GOOGLE_CLIENT_SECRET to enable it, or \
                 back up to your own FTP or SFTP server instead.",
            );
        }
        DriveAccount::SignedOut => {
            setting_row(
                ui,
                "Back up your omp config to Google Drive",
                "Settings, models, custom agents, skills, … and prompt history",
                |ui| {
                    if button(
                        ui,
                        true,
                        DraftKind::Primary,
                        ICON_LOG_IN,
                        "Sign in with Google",
                    )
                    .clicked()
                    {
                        actions.push(BackupAction::SignIn);
                    }
                },
            );
            note(
                ui,
                "Stored in a private app folder only OmpDeck can see. Logins and sessions stay on \
                 this computer.",
            );
            ui.add_space(4.0);
            setting_row(ui, "Backups on Google Drive", MANAGE_SUB, |ui| {
                button(
                    ui,
                    false,
                    DraftKind::Secondary,
                    ICON_LIST_CHECKS,
                    "Manage backups…",
                )
                .on_disabled_hover_text("Sign in with Google first");
            });
        }
        DriveAccount::Waiting => setting_row(
            ui,
            "Waiting for the browser…",
            "Finish signing in on the Google page that just opened.",
            |ui| {
                if button(ui, true, DraftKind::Secondary, ICON_X, "Cancel").clicked() {
                    actions.push(BackupAction::CancelSignIn);
                }
                ui.add(Spin::new(18.0, pal.accent_b));
            },
        ),
        DriveAccount::SignedIn { email } => {
            let who = email
                .as_deref()
                .map_or_else(|| "Signed in".to_owned(), |e| format!("Signed in as {e}"));
            setting_row(
                ui,
                &who,
                "Backups live in OmpDeck's private app folder on your Drive",
                |ui| {
                    if state.busy == Some(Busy::SigningOut) {
                        ui.add(Spin::new(18.0, pal.accent_b));
                    } else if button(
                        ui,
                        state.busy.is_none(),
                        DraftKind::Secondary,
                        ICON_LOG_OUT,
                        "Sign out",
                    )
                    .clicked()
                    {
                        actions.push(BackupAction::SignOut);
                    }
                },
            );
        }
    }
}

/// `user (password) · folder backups` of a saved server.
fn server_detail(server: &ServerView) -> String {
    let who = match (&server.user, &server.key_path) {
        (None, _) => "anonymous".to_owned(),
        (Some(u), Some(_)) => format!("{u} (key file)"),
        (Some(u), None) => format!("{u} (password)"),
    };
    format!("{who} · folder {}", server.folder)
}

fn server_summary(
    ui: &mut Ui,
    state: &BackupCard,
    server: &ServerView,
    form: &mut ServerForm,
    actions: &mut Vec<BackupAction>,
) {
    let pal = Palette::of_ui(ui);
    let idle = state.busy.is_none();
    setting_row(ui, &server.label, &server_detail(server), |ui| {
        if state.busy == Some(Busy::RemovingServer) {
            ui.add(Spin::new(18.0, pal.accent_b));
        } else if button(
            ui,
            idle,
            DraftKind::Secondary,
            ICON_TRASH_2,
            "Remove server",
        )
        .on_hover_text("Forget this server here; nothing on the server is deleted")
        .clicked()
        {
            actions.push(BackupAction::RemoveServer);
        }
        if button(ui, idle, DraftKind::Secondary, ICON_PENCIL, "Edit").clicked() {
            *form = ServerForm::edit(server);
        }
        if server.pinned {
            badge::show(ui, Some(ICON_LOCK), "trusted", Hue::Green, 10.5).on_hover_text(
                "The server's key or certificate is pinned: a different one is refused",
            );
        }
    });
    if server.protocol == Protocol::Ftp {
        warning(ui, FTP_WARNING, pal.warn);
    }
}

/// A single-line field in a setting row.
fn field(ui: &mut Ui, text: &mut String, hint: &str, width: f32, password: bool) {
    ui.add(
        TextEdit::singleline(text)
            .hint_text(hint)
            .password(password)
            .desired_width(width),
    );
}

fn server_form(
    ui: &mut Ui,
    state: &BackupCard,
    form: &mut ServerForm,
    actions: &mut Vec<BackupAction>,
) {
    let pal = Palette::of_ui(ui);
    let connecting = state.busy == Some(Busy::Connecting);
    let idle = state.busy.is_none();
    ui.add_enabled_ui(!connecting, |ui| {
        setting_row(
            ui,
            "Protocol",
            "SFTP (over SSH), FTPS (FTP over TLS) or plain FTP",
            |ui| {
                let all = [Protocol::Ftp, Protocol::Ftps, Protocol::Sftp];
                let mut i = all.iter().position(|p| *p == form.protocol).unwrap_or(2);
                if segmented(
                    ui,
                    &mut i,
                    &[
                        (ICON_TRIANGLE_ALERT, "FTP"),
                        (ICON_LOCK, "FTPS"),
                        (ICON_LOCK, "SFTP"),
                    ],
                ) {
                    form.set_protocol(all[i]);
                }
            },
        );
        if form.protocol == Protocol::Ftp {
            warning(ui, FTP_WARNING, pal.warn);
            ui.add_space(4.0);
        }
        setting_row(ui, "Server", "Host name or address, and port", |ui| {
            field(ui, &mut form.port, "port", 56.0, false);
            ui.label(RichText::new(":").color(pal.weak));
            field(ui, &mut form.host, "nas.local", FIELD_W - 64.0, false);
        });
        let sftp = form.protocol == Protocol::Sftp;
        if !sftp {
            setting_row(
                ui,
                "Anonymous login",
                "Log in as \"anonymous\" (public FTP servers)",
                |ui| {
                    toggle(ui, &mut form.anonymous);
                },
            );
        }
        let anonymous = form.anonymous && !sftp;
        if !anonymous {
            setting_row(ui, "User name", "Your login on the server", |ui| {
                field(ui, &mut form.user, "user", FIELD_W, false);
            });
        }
        if sftp {
            setting_row(
                ui,
                "Sign in with",
                "A password or an SSH private key",
                |ui| {
                    let mut i = usize::from(form.use_key);
                    if segmented(
                        ui,
                        &mut i,
                        &[(ICON_LOCK, "Password"), (ICON_KEY_ROUND, "Key file")],
                    ) {
                        form.use_key = i == 1;
                    }
                },
            );
        }
        let keep = "Leave empty to keep the saved one";
        if sftp && form.use_key {
            setting_row(
                ui,
                "Private key file",
                "OpenSSH or PEM (ed25519, ECDSA, RSA)",
                |ui| {
                    if state.picker
                        && button(ui, true, DraftKind::Secondary, ICON_FOLDER_OPEN, "Browse…")
                            .clicked()
                    {
                        actions.push(BackupAction::BrowseKey);
                    }
                    field(ui, &mut form.key_path, "~/.ssh/id_ed25519", 200.0, false);
                },
            );
            let sub = if form.keep_secret {
                keep
            } else {
                "Leave empty if the key has none"
            };
            setting_row(ui, "Key passphrase", sub, |ui| {
                field(ui, &mut form.passphrase, "passphrase", FIELD_W, true);
            });
        } else if !anonymous {
            let sub = if form.keep_secret {
                keep
            } else {
                "Saved on this computer only, in a private file"
            };
            setting_row(ui, "Password", sub, |ui| {
                field(ui, &mut form.password, "password", FIELD_W, true);
            });
        }
        setting_row(
            ui,
            "Remote folder",
            "Created if missing; absolute when it starts with /",
            |ui| {
                field(ui, &mut form.folder, DEFAULT_FOLDER, FIELD_W, false);
            },
        );
    });
    if let Some(e) = &form.error {
        ui.add_space(4.0);
        warning(ui, e, pal.danger);
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if connecting {
            ui.add(Spin::new(18.0, pal.accent_b));
            ui.label(RichText::new("Connecting and testing write access…").color(pal.weak));
            return;
        }
        let ready = idle && !form.host.trim().is_empty();
        if button(ui, ready, DraftKind::Primary, ICON_LOG_IN, "Connect")
            .on_hover_text("Log in, create the folder if needed, test writing there, then save")
            .on_disabled_hover_text(if idle {
                "Enter the server's host name first"
            } else {
                "Wait for the running backup job to finish"
            })
            .clicked()
        {
            connect(form, None, actions);
        }
        if form.editing && button(ui, true, DraftKind::Secondary, ICON_X, "Cancel").clicked() {
            *form = ServerForm::default();
        }
    });
}

/// Builds the settings and asks to connect, or shows why they cannot work.
fn connect(form: &mut ServerForm, pinned: Option<String>, actions: &mut Vec<BackupAction>) {
    match form.config(pinned) {
        Ok(config) => {
            form.error = None;
            actions.push(BackupAction::Connect {
                config: Box::new(config),
                keep_secret: form.keep_secret,
            });
        }
        Err(e) => form.error = Some(e),
    }
}

/// "Trust this server?" with the presented fingerprint; Trust connects again with it pinned.
fn trust_prompt(
    ctx: &Context,
    state: &BackupCard,
    form: &mut ServerForm,
    actions: &mut Vec<BackupAction>,
) {
    let Some(fingerprint) = form.trust.clone() else {
        return;
    };
    let pal = Palette::of_ctx(ctx);
    let what = if form.protocol == Protocol::Sftp {
        "host key"
    } else {
        "certificate"
    };
    let host = form.host.trim().to_owned();
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "backup-server-trust",
            title: "Trust this server?",
            width: 520.0,
            confirm_label: "Trust",
            confirm_enabled: state.busy.is_none(),
            danger: false,
            enter_confirms: false,
            icon: Some(ICON_LOCK),
            subtitle: Some("First connection: check the fingerprint"),
            tone: Some(pal.warn),
            ..Default::default()
        },
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "OmpDeck has not connected to {host} before and cannot verify its {what}. \
                         Compare this fingerprint with the one the server's owner gives you \
                         (SSH: ssh-keygen -lf on the server's host key)."
                    ))
                    .color(pal.text),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
            ui.add_space(8.0);
            card::kv(ui, 96.0, "Fingerprint", |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(&fingerprint)
                            .monospace()
                            .color(pal.text_strong),
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });
            ui.add_space(6.0);
            note(
                ui,
                "Once trusted, OmpDeck refuses this server if its identity ever changes.",
            );
        },
    );
    match answer {
        Some(true) => {
            form.trust = None;
            connect(form, Some(fingerprint), actions);
        }
        Some(false) => form.trust = None,
        None => {}
    }
}

/// Schedule, last backup + Restore… / Back up now, and the manager: the same for every
/// destination once it is set up.
fn backup_rows(ui: &mut Ui, state: &BackupCard, actions: &mut Vec<BackupAction>) {
    let pal = Palette::of_ui(ui);
    let busy = state.busy;
    let idle = busy.is_none();
    ui.add_space(4.0);
    setting_row(
        ui,
        "Back up automatically",
        "Checks on this schedule and uploads when the backed-up files changed",
        |ui| {
            egui::ComboBox::from_id_salt("backup-interval")
                .selected_text(state.interval.label())
                .show_ui(ui, |ui| {
                    for v in SyncInterval::ALL {
                        if ui
                            .selectable_label(state.interval == v, v.label())
                            .clicked()
                            && state.interval != v
                        {
                            actions.push(BackupAction::SetInterval(v));
                        }
                    }
                });
        },
    );
    ui.add_space(4.0);
    let sub = state
        .last
        .as_deref()
        .unwrap_or("No backup from this computer yet");
    setting_row(ui, "Last backup", sub, |ui| {
        let restoring = matches!(
            busy,
            Some(Busy::Listing | Busy::Preparing | Busy::Restoring)
        );
        if restoring {
            ui.add(Spin::new(18.0, pal.accent_b));
        } else if button(
            ui,
            idle,
            DraftKind::Secondary,
            ICON_ARCHIVE_RESTORE,
            "Restore…",
        )
        .clicked()
        {
            actions.push(BackupAction::OpenRestore);
        }
        if busy == Some(Busy::BackingUp) {
            ui.add(Spin::new(18.0, pal.accent_b));
        } else if button(
            ui,
            idle,
            DraftKind::Primary,
            ICON_CLOUD_UPLOAD,
            "Back up now",
        )
        .clicked()
        {
            actions.push(BackupAction::BackupNow);
        }
    });
    ui.add_space(4.0);
    setting_row(
        ui,
        &format!("Backups on {}", state.place),
        MANAGE_SUB,
        |ui| {
            if busy == Some(Busy::Deleting) {
                ui.add(Spin::new(18.0, pal.accent_b));
            } else if button(
                ui,
                idle,
                DraftKind::Secondary,
                ICON_LIST_CHECKS,
                "Manage backups…",
            )
            .on_disabled_hover_text("Wait for the running backup job to finish")
            .clicked()
            {
                actions.push(BackupAction::OpenManager);
            }
        },
    );
}

/// One backup in the picker, pre-formatted.
pub struct RestoreRow {
    pub when: String,
    pub host: String,
    pub size: String,
}

pub enum RestoreList<'a> {
    Loading,
    Failed(&'a str),
    Rows(&'a [RestoreRow]),
}

pub enum RestorePick {
    Cancel,
    Retry,
    Choose(usize),
    /// The trash button of a row: ask before deleting it from Drive.
    Delete(usize),
}

/// The modal listing backups newest first; `preparing` = the chosen one is being downloaded,
/// `busy` = some backup job runs (no deleting meanwhile); `place` names the destination.
pub fn restore_dialog(
    ctx: &Context,
    list: &RestoreList<'_>,
    selected: &mut Option<usize>,
    preparing: bool,
    busy: bool,
    place: &str,
) -> Option<RestorePick> {
    let mut retry = false;
    let mut delete = None;
    let title = format!("Restore from {place}");
    let subtitle = format!("Your omp config backups on {place}");
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "backup-restore",
            title: &title,
            width: 520.0,
            confirm_label: "Restore…",
            confirm_enabled: selected.is_some() && !preparing,
            danger: false,
            enter_confirms: false,
            icon: Some(ICON_ARCHIVE_RESTORE),
            subtitle: Some(&subtitle),
            ..Default::default()
        },
        |ui| {
            let pal = Palette::of_ui(ui);
            match list {
                RestoreList::Loading => {
                    ui.horizontal(|ui| {
                        ui.add(Spin::new(18.0, pal.accent_b));
                        ui.label(RichText::new("Loading backups…").color(pal.weak));
                    });
                }
                RestoreList::Failed(err) => {
                    ui.add(
                        egui::Label::new(RichText::new(*err).color(pal.danger))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                    ui.add_space(8.0);
                    retry =
                        button(ui, true, DraftKind::Secondary, ICON_REFRESH_CW, "Retry").clicked();
                }
                RestoreList::Rows([]) => {
                    ui.label(RichText::new(format!("No backups on {place} yet.")).color(pal.weak));
                }
                RestoreList::Rows(rows) => {
                    card::kv(ui, 84.0, "Backups", |ui| {
                        badge::show(ui, None, &rows.len().to_string(), Hue::Teal, 11.0);
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Newest first. Pick the backup to restore.").color(pal.weak),
                    );
                    ui.add_space(6.0);
                    ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (i, r) in rows.iter().enumerate() {
                            let font = egui::TextStyle::Body.resolve(ui.style());
                            let mut label = egui::text::LayoutJob::default();
                            for (text, color) in [
                                (r.when.as_str(), pal.text_strong),
                                ("   ", pal.weak),
                                (r.host.as_str(), pal.weak),
                                ("   ", pal.weak),
                                (r.size.as_str(), Hue::Amber.color(pal)),
                            ] {
                                label.append(
                                    text,
                                    0.0,
                                    egui::TextFormat::simple(font.clone(), color),
                                );
                            }
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                if ui
                                    .add_enabled(
                                        !busy,
                                        Button::new(icon_text(ICON_TRASH_2, 14.0, pal.danger))
                                            .min_size(vec2(28.0, 28.0)),
                                    )
                                    .on_hover_text(format!("Delete this backup from {place}"))
                                    .on_disabled_hover_text("A backup job is running")
                                    .clicked()
                                {
                                    delete = Some(i);
                                }
                                let resp = ui.add_enabled(
                                    !preparing,
                                    Button::selectable(*selected == Some(i), label)
                                        .min_size(vec2(ui.available_width(), 28.0)),
                                );
                                if resp.clicked() {
                                    *selected = Some(i);
                                }
                            });
                        }
                    });
                    if preparing {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.add(Spin::new(18.0, pal.accent_b));
                            ui.label(
                                RichText::new("Downloading and checking the backup…")
                                    .color(pal.weak),
                            );
                        });
                    }
                }
            }
        },
    );
    if retry {
        return Some(RestorePick::Retry);
    }
    if let Some(i) = delete {
        return Some(RestorePick::Delete(i));
    }
    match answer {
        Some(true) => selected.map(RestorePick::Choose),
        Some(false) => Some(RestorePick::Cancel),
        None => None,
    }
}

// ------------------------------------------------------------------ backup manager

/// One config backup in the manager, pre-formatted.
#[derive(Clone, Debug, PartialEq)]
pub struct ManagedBackup {
    /// Index into the app's backup list.
    pub index: usize,
    /// `2026-10-22 13:52:01 UTC`.
    pub when: String,
    /// `3h ago`.
    pub age: String,
    pub bytes: u64,
    /// Human size, empty when Drive reported none.
    pub size: String,
    /// The newest backup on Drive.
    pub latest: bool,
    /// `• <when> · <host>` for the confirmation.
    pub confirm_line: String,
}

/// Every backup one PC uploaded, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct HostGroup {
    pub host: String,
    pub this_pc: bool,
    pub versions: Vec<ManagedBackup>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ManagerList {
    Loading,
    Failed(String),
    Ready(Vec<HostGroup>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagerStep {
    Browse,
    /// "Delete permanently?" with the totals.
    Confirm,
    Deleting,
}

/// State of the "Manage settings backups" dialog.
pub struct BackupManager {
    pub step: ManagerStep,
    list: ManagerList,
    /// Checked backups, by index into the app's backup list.
    checked: HashSet<usize>,
    /// (count, bytes) of `checked`, and its footer text; recomputed on change, not per frame.
    totals: (usize, u64),
    totals_label: String,
    /// The user pressed Cancel while deleting.
    pub cancelling: bool,
    /// `<backup>: <error>` of the last deletion run that failed for some.
    pub failures: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum ManagerAction {
    /// Close (browse), stop deleting (deleting).
    Cancel,
    Refresh,
    Toggle(usize, bool),
    ToggleGroup(usize, bool),
    /// Go to the confirmation.
    AskDelete,
    /// Back from the confirmation to the list.
    Back,
    Delete,
}

impl BackupManager {
    pub fn new() -> BackupManager {
        let mut m = BackupManager {
            step: ManagerStep::Browse,
            list: ManagerList::Loading,
            checked: HashSet::new(),
            totals: (0, 0),
            totals_label: String::new(),
            cancelling: false,
            failures: Vec::new(),
        };
        m.recount();
        m
    }

    /// (checked backups, their total size).
    pub fn totals(&self) -> (usize, u64) {
        self.totals
    }

    /// The checked indices into the app's backup list, ascending (= newest first).
    pub fn checked(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.checked.iter().copied().collect();
        v.sort_unstable();
        v
    }

    pub fn set_version(&mut self, index: usize, on: bool) {
        if on {
            self.checked.insert(index);
        } else {
            self.checked.remove(&index);
        }
        self.recount();
    }

    pub fn set_group(&mut self, group: usize, on: bool) {
        let ManagerList::Ready(groups) = &self.list else {
            return;
        };
        let Some(g) = groups.get(group) else {
            return;
        };
        for v in &g.versions {
            if on {
                self.checked.insert(v.index);
            } else {
                self.checked.remove(&v.index);
            }
        }
        self.recount();
    }

    /// Replaces the list. Selections are indices into the app's list, which a refresh renumbers,
    /// so they are dropped.
    pub fn set_list(&mut self, list: ManagerList) {
        self.checked.clear();
        self.list = list;
        self.recount();
    }

    fn recount(&mut self) {
        self.totals = match &self.list {
            ManagerList::Ready(groups) => groups
                .iter()
                .flat_map(|g| &g.versions)
                .filter(|v| self.checked.contains(&v.index))
                .fold((0, 0), |(n, b), v| (n + 1, b + v.bytes)),
            _ => (0, 0),
        };
        self.totals_label = format!(
            "{} selected · {}",
            self.totals.0,
            crate::app::human_size(self.totals.1)
        );
    }
}

impl Default for BackupManager {
    fn default() -> Self {
        Self::new()
    }
}

/// "Manage settings backups": every config backup on `place`, PC → versions, with checkboxes.
/// `progress` = (deleted so far, total) while deleting; `busy` = another backup job runs.
pub fn manager_dialog(
    ctx: &Context,
    d: &BackupManager,
    progress: Option<(usize, usize)>,
    busy: bool,
    place: &str,
) -> Option<ManagerAction> {
    let pal = Palette::of_ctx(ctx);
    let mut act = None;
    let (selected, bytes) = d.totals;
    let (label, enabled, danger) = match d.step {
        ManagerStep::Browse => (
            "Delete selected…",
            selected > 0 && !busy && matches!(d.list, ManagerList::Ready(_)),
            false,
        ),
        ManagerStep::Confirm => ("Delete permanently", true, true),
        ManagerStep::Deleting => ("Delete permanently", false, true),
    };
    let cancel_label = match (&d.step, &d.list) {
        (ManagerStep::Browse, ManagerList::Failed(_)) => "Close",
        (ManagerStep::Browse, ManagerList::Ready(_)) if selected == 0 => "Close",
        (ManagerStep::Confirm, _) => "Back",
        _ => "Cancel",
    };
    let answer = show_dialog_labeled(
        ctx,
        &Dialog {
            id: "backup-manager",
            title: "Manage settings backups",
            width: 560.0,
            confirm_label: label,
            confirm_enabled: enabled,
            danger,
            enter_confirms: false,
            icon: Some(if danger {
                ICON_TRASH_2
            } else {
                ICON_LIST_CHECKS
            }),
            subtitle: Some(&match d.step {
                ManagerStep::Browse => {
                    "Old versions and other PCs' backups (newest 30 kept)".to_owned()
                }
                ManagerStep::Confirm => "Permanent — cannot be undone".to_owned(),
                ManagerStep::Deleting => format!("Deleting from {place}…"),
            }),
            ..Default::default()
        },
        cancel_label,
        |ui| match d.step {
            ManagerStep::Browse => manager_body(ui, d, busy, place, &mut act),
            ManagerStep::Confirm => confirm_body(ui, d, selected, bytes, place),
            ManagerStep::Deleting => {
                let text = if d.cancelling {
                    "Stopping after the current backup…".to_owned()
                } else if let Some((done, total)) = progress {
                    format!("Deleting {} of {total}", (done + 1).min(total))
                } else {
                    "Deleting…".to_owned()
                };
                ui.horizontal(|ui| {
                    ui.add(Spin::new(16.0, pal.accent_b));
                    ui.label(RichText::new(text).color(pal.weak));
                });
                if let Some((done, total)) = progress {
                    ui.add(
                        egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                            .desired_height(6.0)
                            .desired_width(ui.available_width()),
                    );
                }
            }
        },
    );
    if act.is_some() {
        return act;
    }
    match (answer, d.step) {
        (Some(true), ManagerStep::Browse) => Some(ManagerAction::AskDelete),
        (Some(true), ManagerStep::Confirm) => Some(ManagerAction::Delete),
        (Some(false), ManagerStep::Confirm) => Some(ManagerAction::Back),
        (Some(false), _) => Some(ManagerAction::Cancel),
        _ => None,
    }
}

fn wrapped(ui: &mut Ui, text: RichText) {
    ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Wrap));
}

fn confirm_body(ui: &mut Ui, d: &BackupManager, selected: usize, bytes: u64, place: &str) {
    let pal = Palette::of_ui(ui);
    wrapped(
        ui,
        RichText::new(format!(
            "Permanently delete these settings backups from {place}?"
        ))
        .color(pal.text_strong),
    );
    ui.add_space(6.0);
    card::kv(ui, 84.0, "Backups", |ui| {
        badge::show(ui, None, &selected.to_string(), Hue::Amber, 11.0);
    });
    card::kv(ui, 84.0, "Size", |ui| {
        badge::show(ui, None, &crate::app::human_size(bytes), Hue::Teal, 11.0);
    });
    card::kv(ui, 84.0, "Target", |ui| {
        badge::show(ui, Some(ICON_CLOUD), place, Hue::Blue, 11.0);
    });
    ui.add_space(4.0);
    wrapped(
        ui,
        RichText::new("This can't be undone. The files do not go to the trash.").color(pal.danger),
    );
    let ManagerList::Ready(groups) = &d.list else {
        return;
    };
    ui.add_space(8.0);
    card::section(ui, "BACKUPS TO DELETE");
    ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
        for v in groups
            .iter()
            .flat_map(|g| &g.versions)
            .filter(|v| d.checked.contains(&v.index))
        {
            ui.add(
                egui::Label::new(RichText::new(&v.confirm_line).small().color(pal.text)).truncate(),
            );
        }
    });
}

fn manager_body(
    ui: &mut Ui,
    d: &BackupManager,
    busy: bool,
    place: &str,
    act: &mut Option<ManagerAction>,
) {
    let pal = Palette::of_ui(ui);
    let groups = match &d.list {
        ManagerList::Loading => {
            ui.horizontal(|ui| {
                ui.add(Spin::new(18.0, pal.accent_b));
                ui.label(RichText::new("Loading backups…").color(pal.weak));
            });
            return;
        }
        ManagerList::Failed(e) => {
            wrapped(ui, RichText::new(e).color(pal.danger));
            ui.add_space(8.0);
            if button(ui, true, DraftKind::Secondary, ICON_REFRESH_CW, "Retry").clicked() {
                *act = Some(ManagerAction::Refresh);
            }
            return;
        }
        ManagerList::Ready(g) => g,
    };
    if !d.failures.is_empty() {
        let n = d.failures.len();
        wrapped(
            ui,
            RichText::new(format!(
                "{n} backup{} could not be deleted:",
                if n == 1 { "" } else { "s" }
            ))
            .color(pal.danger),
        );
        for f in &d.failures {
            wrapped(ui, RichText::new(f).small().color(pal.danger));
        }
        ui.add_space(6.0);
    }
    if groups.is_empty() {
        ui.label(RichText::new(format!("No settings backups on {place}.")).color(pal.weak));
        ui.add_space(8.0);
        if button(ui, !busy, DraftKind::Secondary, ICON_REFRESH_CW, "Refresh").clicked() {
            *act = Some(ManagerAction::Refresh);
        }
        return;
    }
    wrapped(
        ui,
        RichText::new("Grouped by the PC that uploaded them, newest first.").color(pal.weak),
    );
    ui.add_space(6.0);
    ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        for (gi, g) in groups.iter().enumerate() {
            if gi > 0 {
                ui.add_space(6.0);
            }
            let mut on = g.versions.iter().all(|v| d.checked.contains(&v.index));
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut on, "")
                    .on_hover_text("Select every backup from this PC")
                    .changed()
                {
                    *act = Some(ManagerAction::ToggleGroup(gi, on));
                }
                ui.label(icon_text(ICON_HARD_DRIVE, 16.0, pal.glyph));
                ui.label(theme::strong(&g.host).color(pal.text_strong));
                if g.this_pc {
                    badge::show(ui, None, "this PC", Hue::Blue, 10.5);
                }
                {
                    let n = g.versions.len();
                    badge::show(
                        ui,
                        None,
                        &format!("{n} version{}", if n == 1 { "" } else { "s" }),
                        Hue::Teal,
                        10.5,
                    );
                }
            });
            ui.indent(("drive-mgr-versions", gi), |ui| {
                for v in &g.versions {
                    let mut on = d.checked.contains(&v.index);
                    ui.horizontal(|ui| {
                        if ui
                            .checkbox(&mut on, RichText::new(&v.when).small().color(pal.text))
                            .changed()
                        {
                            *act = Some(ManagerAction::Toggle(v.index, on));
                        }
                        ui.label(RichText::new(&v.age).small().color(pal.weak));
                        if !v.size.is_empty() {
                            badge::show(ui, None, &v.size, Hue::Teal, 10.5);
                        }
                        if v.latest {
                            badge::show(ui, None, "latest", Hue::Green, 10.5)
                                .on_hover_text(format!("The newest settings backup on {place}"));
                        }
                    });
                }
            });
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if button(ui, !busy, DraftKind::Secondary, ICON_REFRESH_CW, "Refresh").clicked() {
            *act = Some(ManagerAction::Refresh);
        }
        ui.add_space(4.0);
        ui.label(RichText::new(&d.totals_label).color(pal.text));
    });
    if busy {
        wrapped(
            ui,
            RichText::new("Another backup job is running; wait for it to finish.")
                .small()
                .color(pal.weak),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_form_builds_the_config_and_follows_the_default_port() {
        let mut form = ServerForm {
            host: "nas".into(),
            user: "me".into(),
            password: "pw".into(),
            ..ServerForm::default()
        };
        form.set_protocol(Protocol::Ftps);
        assert_eq!(form.port, "21");
        form.port = "2121".into();
        form.set_protocol(Protocol::Ftp);
        assert_eq!(form.port, "2121", "a typed port stays");
        let c = form.config(Some("fp".into())).expect("valid");
        assert_eq!(
            (c.protocol, c.port, c.pinned.as_deref()),
            (Protocol::Ftp, 2121, Some("fp"))
        );
        assert!(matches!(&c.auth, ServerAuth::Password { password, .. } if password == "pw"));

        form.anonymous = true;
        assert_eq!(
            form.config(None).expect("valid").auth,
            ServerAuth::Anonymous
        );
        form.port = "0".into();
        assert!(form.config(None).is_err());
        form.port = String::new();
        assert_eq!(form.config(None).expect("valid").port, 0);
    }

    #[test]
    fn server_form_needs_a_secret_unless_editing_and_keeps_sftp_key_auth() {
        let mut form = ServerForm {
            host: "box".into(),
            user: "me".into(),
            ..ServerForm::default()
        };
        assert!(
            form.config(None).is_err(),
            "a new server needs its password"
        );
        form.use_key = true;
        assert!(form.config(None).is_err(), "key auth needs the file");
        form.key_path = " /k/id_ed25519 ".into();
        let c = form.config(None).expect("valid");
        assert_eq!(
            c.auth,
            ServerAuth::Key {
                user: "me".into(),
                key_path: PathBuf::from("/k/id_ed25519"),
                passphrase: None,
            }
        );

        let view = ServerView {
            protocol: Protocol::Sftp,
            host: "box".into(),
            port: 22,
            user: Some("me".into()),
            key_path: None,
            folder: "b".into(),
            pinned: true,
            label: "sftp://box".into(),
        };
        let edit = ServerForm::edit(&view);
        assert!(edit.editing && edit.keep_secret && !edit.use_key);
        assert!(edit.password.is_empty());
        assert!(matches!(
            edit.config(None)
                .expect("an empty password keeps the saved one")
                .auth,
            ServerAuth::Password { .. }
        ));
    }

    fn version(index: usize, bytes: u64) -> ManagedBackup {
        ManagedBackup {
            index,
            when: format!("t{index}"),
            age: String::new(),
            bytes,
            size: String::new(),
            latest: index == 0,
            confirm_line: String::new(),
        }
    }

    #[test]
    fn manager_selection_totals_groups_and_resets_on_refresh() {
        let mut d = BackupManager::new();
        d.set_list(ManagerList::Ready(vec![
            HostGroup {
                host: "me".into(),
                this_pc: true,
                versions: vec![version(0, 10), version(2, 20)],
            },
            HostGroup {
                host: "other".into(),
                this_pc: false,
                versions: vec![version(1, 5)],
            },
        ]));
        assert_eq!(d.totals(), (0, 0));
        d.set_group(0, true);
        assert_eq!(d.totals(), (2, 30));
        d.set_version(1, true);
        d.set_version(2, false);
        assert_eq!(d.totals(), (2, 15));
        assert_eq!(d.checked(), vec![0, 1]);
        assert_eq!(d.totals_label, "2 selected · 15 B");
        d.set_group(0, false);
        assert_eq!(d.checked(), vec![1]);
        d.set_list(ManagerList::Loading);
        assert_eq!(d.totals(), (0, 0));
        assert!(d.checked().is_empty());
    }
}
