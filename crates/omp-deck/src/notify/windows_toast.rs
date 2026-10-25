//! Windows notifier: WinRT toasts built directly (`windows` crate) so each one carries a tag and
//! group and can be withdrawn from the Action Center through the toast history. Toasts need an
//! AppUserModelID the shell knows: the Start-menu shortcut written by `install.ps1` carries ours;
//! without it (portable copy, older install) the PowerShell id is used so toasts still appear.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastDismissalReason, ToastDismissedEventArgs, ToastNotification, ToastNotificationManager,
};
use windows::core::{HSTRING, IInspectable};
use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

use super::toast_xml::{self, GROUP};
use super::{AUMID, Cmd, Out, Request, Signal, lnk_has_aumid};

/// Toasts sent under this id appear as coming from PowerShell; the fallback when the shell does
/// not know [`AUMID`].
const POWERSHELL_APP_ID: &str =
    "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";

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
        POWERSHELL_APP_ID
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

type Sink = Arc<dyn Fn(Signal) + Send + Sync>;

/// A tagged, grouped toast for `req`; a click reports [`Signal::Clicked`], a dismissal by the
/// user or a timeout [`Signal::Gone`]. Withdrawals by the app (`ApplicationHidden`, which also
/// covers a toast replaced by a newer one with the same tag) are not reported: the sender
/// already knows.
fn build_toast(req: &Request, sink: &Sink) -> windows::core::Result<ToastNotification> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(toast_xml::document(&req.title, &req.body)))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    toast.SetTag(&HSTRING::from(toast_xml::tag(req.tab)))?;
    toast.SetGroup(&HSTRING::from(GROUP))?;
    let tab = req.tab;
    let clicked = sink.clone();
    toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
        move |_, _| {
            clicked(Signal::Clicked { tab, token: None });
            Ok(())
        },
    ))?;
    let dismissed = sink.clone();
    toast.Dismissed(&TypedEventHandler::<
        ToastNotification,
        ToastDismissedEventArgs,
    >::new(move |_, args| {
        let hidden_by_app = args.as_ref().and_then(|a| a.Reason().ok())
            == Some(ToastDismissalReason::ApplicationHidden);
        if !hidden_by_app {
            dismissed(Signal::Gone { tab });
        }
        Ok(())
    }))?;
    Ok(toast)
}

fn show(app_id: &str, req: &Request, sink: &Sink) -> windows::core::Result<()> {
    let toast = build_toast(req, sink)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id))?.Show(&toast)
}

/// Removes the notification of `tab` from the screen and the Action Center.
fn withdraw(app_id: &str, tab: Option<u64>) -> windows::core::Result<()> {
    ToastNotificationManager::History()?.RemoveGroupedTagWithId(
        &HSTRING::from(toast_xml::tag(tab)),
        &HSTRING::from(GROUP),
        &HSTRING::from(app_id),
    )
}

/// Removes every OmpDeck notification.
fn withdraw_all(app_id: &str) -> windows::core::Result<()> {
    ToastNotificationManager::History()?
        .RemoveGroupWithId(&HSTRING::from(GROUP), &HSTRING::from(app_id))
}

pub fn run(cmds: Receiver<Cmd>, out: &Out) {
    let app_id = toast_app_id();
    let sink: Sink = {
        let out = out.clone();
        Arc::new(move |s| out.send(s))
    };
    let mut reported = false;
    for cmd in cmds.iter() {
        let result = match cmd {
            Cmd::Show(req) => show(app_id, &req, &sink),
            Cmd::Close(tab) => withdraw(app_id, tab),
            Cmd::CloseAll => withdraw_all(app_id),
        };
        if let Err(e) = result {
            log::warn!("toast operation failed: {e}");
            if !reported {
                reported = true;
                out.send(Signal::Unavailable(format!("the toast failed: {e}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Toasts need a desktop session that accepts them, which CI runners and headless boxes lack;
    /// run on one with `cargo test -p omp-deck -- --ignored windows_toast`.
    #[test]
    #[ignore = "needs an interactive Windows desktop (toast platform); run with --ignored"]
    fn windows_toast_show_then_withdraw_by_tag() {
        let app_id = POWERSHELL_APP_ID;
        let tab = Some(987_654_321);
        let tag = toast_xml::tag(tab);
        let sink: Sink = Arc::new(|_| {});
        let req = Request {
            tab,
            title: "OmpDeck <test> & \"quotes\"".to_owned(),
            body: "withdrawal test".to_owned(),
        };
        let in_history = || -> bool {
            let history = ToastNotificationManager::History()
                .and_then(|h| h.GetHistoryWithId(&HSTRING::from(app_id)))
                .expect("reading the toast history");
            history.into_iter().any(|t| {
                t.Tag().is_ok_and(|v| v == tag.as_str()) && t.Group().is_ok_and(|v| v == GROUP)
            })
        };
        withdraw(app_id, tab).expect("clearing leftovers");
        show(app_id, &req, &sink).expect("showing the toast");
        assert!(in_history(), "the toast should be in the history");
        withdraw(app_id, tab).expect("withdrawing the toast");
        assert!(!in_history(), "the toast should be withdrawn");

        show(app_id, &req, &sink).expect("showing the toast again");
        assert!(in_history());
        withdraw_all(app_id).expect("withdrawing the group");
        assert!(!in_history(), "the group should be cleared");
    }
}
