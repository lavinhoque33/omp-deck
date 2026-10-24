//! Windows notifier: WinRT toasts through `tauri-winrt-notification`. Toasts need an
//! AppUserModelID the shell knows: the Start-menu shortcut written by `install.ps1` carries ours;
//! without it (portable copy, older install) the PowerShell id is used so toasts still appear.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use tauri_winrt_notification::Toast;
use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

use super::{AUMID, Cmd, Out, Signal, lnk_has_aumid};

/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\OmpDeck.lnk`, where `install.ps1` puts it.
fn shortcut_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs")
            .join("OmpDeck.lnk"),
    )
}

/// The id toasts are sent under.
fn toast_app_id() -> &'static str {
    let has = shortcut_path()
        .and_then(|p| std::fs::read(p).ok())
        .is_some_and(|bytes| lnk_has_aumid(&bytes, AUMID));
    if has {
        AUMID
    } else {
        log::info!("the Start-menu shortcut has no AppUserModelID: toasts use the PowerShell id");
        Toast::POWERSHELL_APP_ID
    }
}

pub fn set_process_aumid() {
    let wide: Vec<u16> = AUMID.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
    let hr = unsafe { SetCurrentProcessExplicitAppUserModelID(wide.as_ptr()) };
    if hr < 0 {
        log::warn!("SetCurrentProcessExplicitAppUserModelID failed: {hr:#x}");
    }
}

pub fn run(cmds: Receiver<Cmd>, out: &Out) {
    let app_id = toast_app_id();
    let mut reported = false;
    for cmd in cmds.iter() {
        // A shown toast cannot be withdrawn through this crate, so `Show` is the only command here.
        let Cmd::Show(req) = cmd;
        let (clicked, dismissed) = (out.clone(), out.clone());
        let tab = req.tab;
        let shown = Toast::new(app_id)
            .title(&req.title)
            .text1(&req.body)
            .on_activated(move |_| {
                clicked.send(Signal::Clicked { tab, token: None });
                Ok(())
            })
            .on_dismissed(move |_| {
                dismissed.send(Signal::Gone { tab });
                Ok(())
            })
            .show();
        if let Err(e) = shown
            && !reported
        {
            reported = true;
            out.send(Signal::Unavailable(format!(
                "showing the toast failed: {e}"
            )));
        }
    }
}
