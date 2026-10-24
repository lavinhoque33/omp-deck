//! Linux notifier: `org.freedesktop.Notifications` over one session-bus connection. One thread
//! runs the commands (Notify / CloseNotification / the Wayland raise), a second reads the server's
//! signals in arrival order so `ActivationToken` is always seen before its `ActionInvoked`.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_lite::StreamExt;
use zbus::message::Type;
use zbus::zvariant::Value;
use zbus::{Connection, MatchRule, Message, MessageStream};

use super::wayland::Raiser;
use super::{Cmd, Out, Request, Signal};

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// How long a D-Bus call may take before the notifier gives up on it.
const METHOD_TIMEOUT: Duration = Duration::from_secs(5);
const APP_NAME: &str = "OmpDeck";
/// Matches `packaging/omp-deck.desktop` and the window's app_id: the shell groups and styles by it.
const DESKTOP_ENTRY: &str = "omp-deck";
/// Icon name of the installed hicolor icon.
const ICON: &str = "omp-deck";

#[derive(Default)]
struct Shared {
    by_tab: HashMap<Option<u64>, u32>,
    by_id: HashMap<u32, Option<u64>>,
    /// `ActivationToken` seen, `ActionInvoked` not yet.
    tokens: HashMap<u32, String>,
}

type State = Arc<Mutex<Shared>>;

fn lock(state: &State) -> MutexGuard<'_, Shared> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn run(cmds: Receiver<Cmd>, out: &Out) {
    // A bounded method timeout: with the notification service gone, the bus daemon's activation
    // attempt can take a minute, which would stall this thread (and every queued command).
    let built = zbus::block_on(async {
        zbus::connection::Builder::session()?
            .method_timeout(METHOD_TIMEOUT)
            .build()
            .await
    });
    let conn = match built {
        Ok(c) => c,
        Err(e) => {
            unavailable(out, &format!("no D-Bus session bus: {e}"));
            for _ in cmds.iter() {}
            return;
        }
    };
    let state: State = Arc::default();
    {
        let (conn, state, out) = (conn.clone(), state.clone(), out.clone());
        let spawned = std::thread::Builder::new()
            .name("omp-deck-notify-signals".into())
            .spawn(move || listen(&conn, &state, &out));
        if let Err(e) = spawned {
            log::warn!("starting the notification signal thread failed: {e}");
        }
    }
    let mut raiser: Option<Raiser> = None;
    let mut reported = false;
    while let Ok(first) = cmds.recv() {
        // Whatever piled up behind a slow call: only the newest notification per tab is worth showing.
        let batch: Vec<Cmd> = std::iter::once(first).chain(cmds.try_iter()).collect();
        for cmd in coalesce(batch) {
            match cmd {
                Cmd::Show(req) => {
                    if let Err(e) = zbus::block_on(notify(&conn, &state, &req))
                        && !reported
                    {
                        reported = true;
                        unavailable(out, &e);
                    }
                }
                Cmd::Close(tab) => zbus::block_on(close(&conn, &state, tab)),
                Cmd::Window { display, surface } => match Raiser::new(display, surface) {
                    Ok(r) => raiser = Some(r),
                    Err(e) => log::info!("Wayland window raise unavailable: {e}"),
                },
                Cmd::Raise(token) => {
                    let done = raiser.as_ref().map(|r| r.activate(&token));
                    match done {
                        Some(Ok(())) => {}
                        Some(Err(e)) => {
                            log::warn!("xdg-activation failed: {e}");
                            out.send(Signal::RaiseFailed);
                        }
                        None => out.send(Signal::RaiseFailed),
                    }
                }
            }
        }
    }
}

/// Drops every `Show` that a later `Show` for the same tab supersedes (the same notification would
/// be replaced right away); tab-less test notifications and all other commands are kept in order.
fn coalesce(batch: Vec<Cmd>) -> Vec<Cmd> {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<Cmd> = batch
        .into_iter()
        .rev()
        .filter(|c| match c {
            Cmd::Show(Request { tab: Some(t), .. }) => seen.insert(*t),
            _ => true,
        })
        .collect();
    kept.reverse();
    kept
}

fn unavailable(out: &Out, why: &str) {
    log::info!("desktop notifications unavailable: {why}");
    out.send(Signal::Unavailable(why.to_owned()));
}

async fn notify(conn: &Connection, state: &State, req: &Request) -> Result<(), String> {
    let replaces = lock(state).by_tab.get(&req.tab).copied().unwrap_or(0);
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("desktop-entry", Value::from(DESKTOP_ENTRY));
    let body = (
        APP_NAME,
        replaces,
        ICON,
        req.title.as_str(),
        req.body.as_str(),
        vec!["default", "Open"],
        hints,
        -1i32,
    );
    let reply = conn
        .call_method(Some(SERVICE), PATH, Some(SERVICE), "Notify", &body)
        .await
        .map_err(|e| format!("Notify failed: {e}"))?;
    let id: u32 = reply
        .body()
        .deserialize()
        .map_err(|e| format!("unexpected Notify reply: {e}"))?;
    let mut s = lock(state);
    if let Some(old) = s.by_tab.insert(req.tab, id)
        && old != id
    {
        s.by_id.remove(&old);
        s.tokens.remove(&old);
    }
    s.by_id.insert(id, req.tab);
    Ok(())
}

async fn close(conn: &Connection, state: &State, tab: Option<u64>) {
    let id = {
        let mut s = lock(state);
        let id = s.by_tab.remove(&tab);
        if let Some(id) = id {
            s.by_id.remove(&id);
            s.tokens.remove(&id);
        }
        id
    };
    if let Some(id) = id
        && let Err(e) = conn
            .call_method(
                Some(SERVICE),
                PATH,
                Some(SERVICE),
                "CloseNotification",
                &(id,),
            )
            .await
    {
        log::debug!("CloseNotification({id}) failed: {e}");
    }
}

fn listen(conn: &Connection, state: &State, out: &Out) {
    let result = zbus::block_on(async {
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(SERVICE)?
            .path(PATH)?
            .build();
        let mut stream = MessageStream::for_match_rule(rule, conn, None).await?;
        while let Some(msg) = stream.next().await {
            if let Ok(msg) = msg {
                on_signal(&msg, state, out);
            }
        }
        Ok::<(), zbus::Error>(())
    });
    if let Err(e) = result {
        log::info!("notification signal stream ended: {e}");
    }
}

fn on_signal(msg: &Message, state: &State, out: &Out) {
    let header = msg.header();
    let Some(member) = header.member() else {
        return;
    };
    let body = msg.body();
    log::debug!("notification signal {}", member.as_str());
    match member.as_str() {
        "ActivationToken" => {
            if let Ok((id, token)) = body.deserialize::<(u32, String)>() {
                let mut s = lock(state);
                if s.by_id.contains_key(&id) {
                    s.tokens.insert(id, token);
                }
            }
        }
        "ActionInvoked" => {
            if let Ok((id, _action)) = body.deserialize::<(u32, String)>() {
                let (tab, token) = {
                    let mut s = lock(state);
                    let Some(tab) = s.by_id.get(&id).copied() else {
                        log::debug!("ActionInvoked for unknown notification {id}");
                        return;
                    };
                    (tab, s.tokens.remove(&id))
                };
                out.send(Signal::Clicked { tab, token });
            }
        }
        "NotificationClosed" => {
            if let Ok((id, _reason)) = body.deserialize::<(u32, u32)>() {
                let tab = {
                    let mut s = lock(state);
                    s.tokens.remove(&id);
                    let tab = s.by_id.remove(&id);
                    if let Some(tab) = tab
                        && s.by_tab.get(&tab) == Some(&id)
                    {
                        s.by_tab.remove(&tab);
                    }
                    tab
                };
                if let Some(tab) = tab {
                    out.send(Signal::Gone { tab });
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(tab: Option<u64>, body: &str) -> Cmd {
        Cmd::Show(Request {
            tab,
            title: String::new(),
            body: body.to_owned(),
        })
    }

    #[test]
    fn only_the_newest_show_per_tab_survives_in_order() {
        let batch = vec![
            show(Some(1), "old"),
            show(None, "test"),
            Cmd::Close(Some(2)),
            show(Some(1), "new"),
            show(Some(2), "other"),
        ];
        let out = coalesce(batch);
        let bodies: Vec<&str> = out
            .iter()
            .map(|c| match c {
                Cmd::Show(r) => r.body.as_str(),
                _ => "close",
            })
            .collect();
        assert_eq!(bodies, ["test", "close", "new", "other"]);
    }
}
