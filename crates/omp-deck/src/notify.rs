//! Desktop notifications: the trigger rules ([`tracker`]), and a notifier thread per platform that
//! shows them (Linux: `org.freedesktop.Notifications` over one zbus connection, raising the window
//! with xdg-activation on Wayland; Windows: WinRT toasts filed under a tag and group, so they can be withdrawn). The UI thread only sends requests and
//! drains signals; it never touches D-Bus or WinRT.

use std::sync::mpsc::{self, Receiver, Sender};

pub mod classify;
pub mod inbox;
pub mod tracker;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(windows, test))]
mod toast_xml;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod windows_toast;

/// The Windows AppUserModelID of OmpDeck (the Start-menu shortcut carries the same one).
#[cfg(any(windows, test))]
pub const AUMID: &str = "io.github.lavinhoque33.OmpDeck";

/// One notification to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The tab it is about (`None` = the settings test); a newer request for the same tab replaces
    /// the older notification.
    pub tab: Option<u64>,
    pub title: String,
    pub body: String,
}

/// What the notifier thread reports back.
#[derive(Debug, PartialEq, Eq)]
pub enum Signal {
    /// The user clicked the notification. `token` is the compositor's xdg-activation token.
    Clicked {
        tab: Option<u64>,
        token: Option<String>,
    },
    /// The notification went away without a click (dismissed, expired, replaced by the server).
    Gone { tab: Option<u64> },
    /// Notifications cannot be shown (no notification service, WinRT failure); sent once.
    Unavailable(String),
    /// A requested raise could not be done by the platform path: use `Focus` + attention instead.
    #[cfg(target_os = "linux")]
    RaiseFailed,
}

enum Cmd {
    Show(Request),
    Close(Option<u64>),
    /// Withdraw every tab's notification (the settings test stays).
    CloseAll,
    /// winit's `wl_display` and the window's `wl_surface` (raw pointers as integers).
    #[cfg(target_os = "linux")]
    Window {
        display: usize,
        surface: usize,
    },
    /// Raise the window with this activation token.
    #[cfg(target_os = "linux")]
    Raise(String),
}

/// Signal sink of the notifier thread: sends and wakes the UI.
#[derive(Clone)]
struct Out {
    tx: Sender<Signal>,
    ctx: egui::Context,
}

impl Out {
    fn send(&self, s: Signal) {
        if self.tx.send(s).is_ok() {
            self.ctx.request_repaint();
        }
    }
}

pub struct Notifier {
    tx: Sender<Cmd>,
    rx: Receiver<Signal>,
}

impl Notifier {
    /// Starts the notifier thread (it connects in the background; this never blocks).
    pub fn start(ctx: egui::Context) -> Notifier {
        let (tx, cmds) = mpsc::channel();
        let (sig_tx, rx) = mpsc::channel();
        let out = Out { tx: sig_tx, ctx };
        let spawned = std::thread::Builder::new()
            .name("omp-deck-notify".into())
            .spawn(move || run(cmds, out));
        if let Err(e) = spawned {
            log::warn!("starting the notification thread failed: {e}");
        }
        Notifier { tx, rx }
    }

    pub fn show(&self, req: Request) {
        let _ = self.tx.send(Cmd::Show(req));
    }

    /// Withdraw the notification of `tab` (best effort).
    pub fn close(&self, tab: Option<u64>) {
        let _ = self.tx.send(Cmd::Close(tab));
    }

    /// Withdraw every tab's notification (best effort); the settings test notification stays.
    pub fn close_all(&self) {
        let _ = self.tx.send(Cmd::CloseAll);
    }

    /// Hand the Wayland window over so a click can raise it (no-op on other windowing systems).
    pub fn attach_window(&self, frame: &eframe::Frame) {
        #[cfg(target_os = "linux")]
        if let Some((display, surface)) = wayland::window_pointers(frame) {
            let _ = self.tx.send(Cmd::Window { display, surface });
        }
        #[cfg(not(target_os = "linux"))]
        let _ = frame;
    }

    /// Ask the platform to raise the window with the activation `token`. Reports
    /// [`Signal::RaiseFailed`] when that path is unavailable. Returns false when it was not even tried.
    pub fn raise_with_token(&self, token: &str) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.tx.send(Cmd::Raise(token.to_owned())).is_ok()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = token;
            false
        }
    }

    pub fn drain(&self) -> Vec<Signal> {
        self.rx.try_iter().collect()
    }
}

#[cfg(target_os = "linux")]
fn run(cmds: Receiver<Cmd>, out: Out) {
    linux::run(cmds, &out);
}

#[cfg(windows)]
fn run(cmds: Receiver<Cmd>, out: Out) {
    windows_toast::run(cmds, &out);
}

#[cfg(not(any(target_os = "linux", windows)))]
fn run(cmds: Receiver<Cmd>, out: Out) {
    out.send(Signal::Unavailable(
        "desktop notifications are not supported on this platform".into(),
    ));
    for _ in cmds.iter() {}
}

/// Windows: tells the shell which AppUserModelID this process belongs to (taskbar grouping and
/// toast identity). Call once at startup.
#[cfg(windows)]
pub fn init_process() {
    windows_toast::set_process_aumid();
}

/// Whether a `.lnk` file carries the `System.AppUserModel.ID` property with `aumid`: the property
/// store is serialized into the shortcut as UTF-16 text.
#[cfg(any(windows, test))]
pub fn lnk_has_aumid(lnk: &[u8], aumid: &str) -> bool {
    let needle: Vec<u8> = aumid.encode_utf16().flat_map(u16::to_le_bytes).collect();
    !needle.is_empty() && lnk.windows(needle.len()).any(|w| w == needle.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lnk_detection_finds_the_utf16_aumid() {
        let mut lnk = b"L\0\0\0 junk ".to_vec();
        lnk.extend(AUMID.encode_utf16().flat_map(u16::to_le_bytes));
        lnk.extend_from_slice(&[0, 0, 7, 7]);
        assert!(lnk_has_aumid(&lnk, AUMID));
        assert!(!lnk_has_aumid(&lnk, "io.github.other.App"));
        assert!(
            !lnk_has_aumid(AUMID.as_bytes(), AUMID),
            "ASCII text is not the stored form"
        );
        assert!(!lnk_has_aumid(b"", AUMID));
    }
}
