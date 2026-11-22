//! The user's current graphical login (display server, X authority, session bus) for the
//! processes the gateway starts.
//!
//! These variables change with every login (KWin may serve `wayland-1` instead of `wayland-0`,
//! the X authority file has a random name), so they are never pinned in a
//! [`StartupContext`](super::StartupContext). A gateway keeps the environment it started with
//! for its whole life, possibly across logins, so every omp session and app window it starts
//! gets the current values instead: those of the connected GUI, else of the systemd user
//! manager (what the desktop session imported), else the gateway's own.

use std::ffi::OsStr;
use std::io;
use std::path::Path;

/// Login-scoped variables.
pub const LOGIN_ENV: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
];

pub fn is_login_var(key: &str) -> bool {
    LOGIN_ENV.contains(&key)
}

/// This process's login variables: what a GUI reports in its hello.
pub fn of_this_process() -> Vec<(String, String)> {
    LOGIN_ENV
        .iter()
        .filter_map(|k| {
            let v = std::env::var(k).ok().filter(|v| !v.is_empty())?;
            Some(((*k).to_owned(), v))
        })
        .collect()
}

/// The login variables a child of this gateway gets: `gui` (a connected GUI's report) when
/// known, else the current login's. Blocking (a D-Bus call on Linux): call on a worker.
pub fn overrides(gui: Option<&[(String, String)]>) -> Vec<(String, String)> {
    let current = match gui {
        Some(env) => env.to_vec(),
        None => current_login(),
    };
    overrides_from(&current, |k| std::env::var(k).ok())
}

/// Every login variable `current` has, plus an empty value for each one only the gateway's own
/// (`inherited`) environment has: a value from an earlier login must not leak through.
fn overrides_from(
    current: &[(String, String)],
    inherited: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    LOGIN_ENV
        .iter()
        .filter_map(|k| {
            match current.iter().find(|(ck, v)| ck == k && !v.is_empty()) {
                Some((_, v)) => Some(v.clone()),
                None => inherited(k)
                    .filter(|v| !v.is_empty())
                    .map(|_| String::new()),
            }
            .map(|v| ((*k).to_owned(), v))
        })
        .collect()
}

/// The login variables of the systemd user manager (the desktop session imports its own there
/// on every login), else this process's.
fn current_login() -> Vec<(String, String)> {
    #[cfg(target_os = "linux")]
    match systemd_environment() {
        Ok(env) => return login_vars_of(&env),
        Err(e) => log::debug!("systemd user environment unavailable: {e}"),
    }
    of_this_process()
}

/// The login variables among `KEY=VALUE` entries.
#[cfg(any(target_os = "linux", test))]
fn login_vars_of(entries: &[String]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter_map(|e| e.split_once('='))
        .filter(|(k, v)| is_login_var(k) && !v.is_empty())
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// `org.freedesktop.systemd1.Manager.Environment` of the user manager.
#[cfg(target_os = "linux")]
fn systemd_environment() -> zbus::Result<Vec<String>> {
    zbus::block_on(async {
        let conn = zbus::connection::Builder::session()?
            .method_timeout(std::time::Duration::from_secs(2))
            .build()
            .await?;
        let reply = conn
            .call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.freedesktop.systemd1.Manager", "Environment"),
            )
            .await?;
        let value: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
        Ok(Vec::<String>::try_from(value)?)
    })
}

/// Starts the native app detached with the current login's variables. Blocking: call on a
/// worker.
#[cfg(unix)]
pub fn launch_app<I, S>(exe: &Path, args: I) -> io::Result<deck_host::Launcher>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in overrides(None) {
        if v.is_empty() {
            cmd.env_remove(k);
        } else {
            cmd.env(k, v);
        }
    }
    cmd.spawn()
}

/// Starts the native app detached (Windows has no login-scoped variables of this kind).
#[cfg(not(unix))]
pub fn launch_app<I, S>(exe: &Path, args: I) -> io::Result<deck_host::Launcher>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    deck_host::launch_detached(exe, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_stale_inherited_display_is_replaced_by_the_current_login() {
        // The gateway started under an earlier login (or from an old startup context).
        let inherited = |k: &str| match k {
            "WAYLAND_DISPLAY" => Some("wayland-0".to_owned()),
            "XAUTHORITY" => Some("/run/user/1000/xauth_old".to_owned()),
            "DISPLAY" => Some(":0".to_owned()),
            _ => None,
        };
        let current = pairs(&[
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("DISPLAY", ":0"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("PATH", "/usr/bin"),
        ]);
        let mut got = overrides_from(&current, inherited);
        got.sort();
        assert_eq!(
            got,
            pairs(&[
                ("DISPLAY", ":0"),
                ("WAYLAND_DISPLAY", "wayland-1"),
                // Gone from the current login: blanked, never the old file.
                ("XAUTHORITY", ""),
                ("XDG_SESSION_TYPE", "wayland"),
            ])
        );
    }

    #[test]
    fn systemd_entries_yield_only_login_variables() {
        let entries = [
            "PATH=/usr/bin",
            "WAYLAND_DISPLAY=wayland-1",
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus",
            "DISPLAY=",
            "garbage",
        ]
        .map(str::to_owned);
        assert_eq!(
            login_vars_of(&entries),
            pairs(&[
                ("WAYLAND_DISPLAY", "wayland-1"),
                ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ])
        );
    }
}
