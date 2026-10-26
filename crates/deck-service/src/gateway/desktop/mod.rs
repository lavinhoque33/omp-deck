//! The gateway's desktop notification adapter: one notifier thread per platform that shows and
//! withdraws the canonical [`crate::notifications::NotificationCenter`]'s pop-ups (Linux:
//! `org.freedesktop.Notifications` over one zbus connection; Windows: WinRT toasts filed under a
//! tag and group so they can be withdrawn). Delivery and activation only: it never decides what to
//! notify. A click is reported with the desktop's activation token (Wayland `xdg-activation`);
//! the gateway forwards it to a native window, which raises itself with it.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::controller::SessionTarget;
pub use crate::notifications::Sound;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(windows, test))]
mod toast_xml;
#[cfg(windows)]
mod windows_toast;

/// The Windows AppUserModelID of OmpDeck (the Start-menu shortcut carries the same one).
#[cfg(any(windows, test))]
pub const AUMID: &str = "io.github.lavinhoque33.OmpDeck";

/// One notification to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The session it is about (`None` = the settings test); a newer request for the same
    /// session replaces the older notification.
    pub tab: Option<SessionTarget>,
    pub title: String,
    pub body: String,
    /// The sound to ask the desktop for; `None` = silent. Linux only: Windows toasts stay silent.
    pub sound: Option<Sound>,
}

/// What the notifier thread reports back.
#[derive(Debug, PartialEq, Eq)]
pub enum Signal {
    /// The user clicked the notification. `token` is the compositor's xdg-activation token.
    Clicked {
        tab: Option<SessionTarget>,
        token: Option<String>,
    },
    /// The notification went away without a click (dismissed, expired, replaced by the server).
    Gone { tab: Option<SessionTarget> },
    /// Notifications cannot be shown (no notification service, WinRT failure); sent once.
    Unavailable(String),
}

enum Cmd {
    Show(Request),
    Close(Option<SessionTarget>),
    /// Withdraw every session's notification (the settings test stays).
    CloseAll,
}

/// Signal sink of the notifier thread: sends and wakes the gateway engine.
#[derive(Clone)]
struct Out {
    tx: Sender<Signal>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Out {
    fn send(&self, s: Signal) {
        if self.tx.send(s).is_ok() {
            (self.wake)();
        }
    }
}

pub struct Notifier {
    tx: Sender<Cmd>,
    rx: Receiver<Signal>,
}

impl Notifier {
    /// Starts the notifier thread (it connects in the background; this never blocks).
    pub fn start(wake: Arc<dyn Fn() + Send + Sync>) -> Notifier {
        let (tx, cmds) = mpsc::channel();
        let (sig_tx, rx) = mpsc::channel();
        let out = Out { tx: sig_tx, wake };
        let spawned = std::thread::Builder::new()
            .name("omp-deck-gateway-notify".into())
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
    pub fn close(&self, tab: Option<SessionTarget>) {
        let _ = self.tx.send(Cmd::Close(tab));
    }

    /// Withdraw every session's notification (best effort); the settings test notification stays.
    pub fn close_all(&self) {
        let _ = self.tx.send(Cmd::CloseAll);
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

/// Windows: tells the shell which AppUserModelID this process belongs to (toast identity). Call
/// once at startup.
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
