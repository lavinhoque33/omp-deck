//! Raising the window when a notification was clicked: the one native effect of a notification.
//! The gateway shows, replaces and withdraws the desktop notifications and reports a click with
//! the desktop's activation token; this window only has to come to the front with it (Wayland:
//! `xdg-activation` on winit's own display, which winit cannot do with an external token).

use std::sync::mpsc::{self, Receiver, Sender};

#[cfg(target_os = "linux")]
pub(crate) mod wayland;

/// The Windows AppUserModelID of OmpDeck (the Start-menu shortcut and the gateway's toasts carry
/// the same one).
#[cfg(windows)]
const AUMID: &str = "io.github.lavinhoque33.OmpDeck";

/// What the raise thread reports back.
#[derive(Debug, PartialEq, Eq)]
pub enum Signal {
    /// A requested raise could not be done by the platform path: use `Focus` + attention instead.
    #[cfg(target_os = "linux")]
    RaiseFailed,
}

enum Cmd {
    /// winit's `wl_display` and the window's `wl_surface` (raw pointers as integers).
    #[cfg(target_os = "linux")]
    Window { display: usize, surface: usize },
    /// Raise the window with this activation token.
    #[cfg(target_os = "linux")]
    Raise(String),
}

/// A thread that owns the platform's raise path, so the UI thread never waits for it.
pub struct WindowRaiser {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    tx: Sender<Cmd>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    rx: Receiver<Signal>,
}

impl WindowRaiser {
    /// Starts the raise thread (it binds the Wayland protocol in the background).
    pub fn start(ctx: egui::Context) -> WindowRaiser {
        let (tx, cmds) = mpsc::channel();
        let (sig_tx, rx) = mpsc::channel();
        #[cfg(target_os = "linux")]
        {
            let spawned = std::thread::Builder::new()
                .name("omp-deck-raise".into())
                .spawn(move || run(cmds, sig_tx, ctx));
            if let Err(e) = spawned {
                log::warn!("starting the window raise thread failed: {e}");
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (cmds, sig_tx, ctx);
        WindowRaiser { tx, rx }
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

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn drain(&self) -> Vec<Signal> {
        self.rx.try_iter().collect()
    }
}

#[cfg(target_os = "linux")]
fn run(cmds: Receiver<Cmd>, out: Sender<Signal>, ctx: egui::Context) {
    let send = |s: Signal| {
        if out.send(s).is_ok() {
            ctx.request_repaint();
        }
    };
    let mut raiser: Option<wayland::Raiser> = None;
    while let Ok(cmd) = cmds.recv() {
        match cmd {
            Cmd::Window { display, surface } => match wayland::Raiser::new(display, surface) {
                Ok(r) => raiser = Some(r),
                Err(e) => log::info!("Wayland window raise unavailable: {e}"),
            },
            Cmd::Raise(token) => match raiser.as_ref().map(|r| r.activate(&token)) {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    log::warn!("xdg-activation failed: {e}");
                    send(Signal::RaiseFailed);
                }
                None => send(Signal::RaiseFailed),
            },
        }
    }
}

/// Windows: tells the shell which AppUserModelID this process belongs to (taskbar grouping and
/// toast identity). Call once at startup.
#[cfg(windows)]
pub fn init_process() {
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    let wide: Vec<u16> = AUMID.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
    let hr = unsafe { SetCurrentProcessExplicitAppUserModelID(wide.as_ptr()) };
    if hr < 0 {
        log::warn!("SetCurrentProcessExplicitAppUserModelID failed: {hr:#x}");
    }
}
