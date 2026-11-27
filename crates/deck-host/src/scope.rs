//! Linux: moves the calling process into a transient systemd scope of its own, so an OOM kill
//! takes down at most the session that used the memory.
//!
//! A desktop launcher starts OmpDeck as one unit (KDE: `app-omp\x2ddeck@….service` with
//! `OOMPolicy=stop` and `KillMode=control-group`), and every process started from it inherits
//! that cgroup: the gateway, each session host, every omp and its tool runs. When the kernel
//! OOM-killed one tool run, systemd stopped the unit and every session died with it
//! (2026-11-19); systemd-oomd likewise kills a whole leaf cgroup. So the gateway and each host
//! call [`enter_own`] once at startup, before they start any child: their scope has
//! `OOMPolicy=continue`, and it is the cgroup oomd picks instead of everything.
//!
//! The scope is made over the user manager's D-Bus API with `busctl` (shipped with systemd, as
//! `systemctl` is for the supervisor unit). Without cgroup v2, outside the user manager's tree,
//! without `busctl` or after a failed call the process stays where it is.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// OmpDeck's application id (`omp-deck.desktop`) escaped for a unit name: desktop tools
/// attribute `app-<ApplicationID>-<RANDOM>.scope` units to the application.
const APP_ID: &str = "omp\\x2ddeck";
/// Limit of the D-Bus call.
const CALL_TIMEOUT: &str = "--timeout=2";
/// How long the scope's start job may take to move this process: the call returns once the
/// job is queued.
const MOVE_WAIT: Duration = Duration::from_secs(1);

/// Moves this process into a new scope `app-omp\x2ddeck-<role>_<pid>_<random>.scope` in the
/// slice it runs in now (`app.slice` when none), with `OOMPolicy=continue`, collected once it is
/// empty or failed. Call before starting any child: only processes started afterwards inherit
/// it. Logs the outcome and never fails the caller.
pub fn enter_own(role: &str, description: &str) {
    let Some(cgroup) = own_cgroup() else {
        log::debug!("no cgroup v2 membership: no systemd scope");
        return;
    };
    // SAFETY: getuid has no failure mode.
    let uid = unsafe { libc::getuid() };
    let Some(slice) = user_manager_slice(&cgroup, uid) else {
        log::debug!("{cgroup} is outside the systemd user manager: no scope");
        return;
    };
    let pid = std::process::id();
    let name = format!("app-{APP_ID}-{role}_{pid}_{}.scope", random_hex());
    let started = busctl(&start_call(&name, description, slice, pid, true)).or_else(|e| {
        if e.contains("OOMPolicy") {
            // systemd < 253: scopes have no OOMPolicy= (and are never stopped on an OOM kill).
            busctl(&start_call(&name, description, slice, pid, false))
        } else {
            Err(e)
        }
    });
    match started {
        Ok(()) if moved_into(&name) => log::info!("running in systemd scope {name}"),
        Ok(()) => log::warn!("systemd scope {name} did not take this process within {MOVE_WAIT:?}"),
        Err(e) => log::warn!("creating systemd scope {name}: {e}"),
    }
}

/// This process's cgroup v2 path.
fn own_cgroup() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    unified(&text).map(str::to_owned)
}

/// The `0::` line of a `/proc/<pid>/cgroup` listing (hybrid hosts list v1 hierarchies too).
fn unified(text: &str) -> Option<&str> {
    text.lines().find_map(|line| line.strip_prefix("0::"))
}

/// The slice for the scope when `cgroup` lies under `user@<uid>.service`, the only tree that
/// user manager may move processes out of: the innermost slice there, else `app.slice`.
fn user_manager_slice(cgroup: &str, uid: u32) -> Option<&str> {
    let manager = format!("user@{uid}.service");
    let mut parts = cgroup.split('/');
    parts.find(|part| *part == manager)?;
    Some(
        parts
            .rfind(|part| part.ends_with(".slice"))
            .unwrap_or("app.slice"),
    )
}

/// Eight hex digits: with the pid they keep the name unique even when a pid comes back while an
/// older scope of it still holds a process.
fn random_hex() -> String {
    let mut bytes = [0u8; 4];
    // Unreadable randomness leaves zeros: the pid still tells live processes apart.
    let _ = getrandom::fill(&mut bytes);
    format!("{:08x}", u32::from_ne_bytes(bytes))
}

/// `busctl` arguments of `Manager.StartTransientUnit(name, "fail", properties, [])` for a scope
/// holding `pid`.
fn start_call(
    name: &str,
    description: &str,
    slice: &str,
    pid: u32,
    oom_policy: bool,
) -> Vec<String> {
    let pid = pid.to_string();
    let mut properties = vec![
        vec!["Description", "s", description],
        vec!["PIDs", "au", "1", &pid],
        vec!["Slice", "s", slice],
        vec!["CollectMode", "s", "inactive-or-failed"],
    ];
    if oom_policy {
        properties.push(vec!["OOMPolicy", "s", "continue"]);
    }
    let count = properties.len().to_string();
    [
        "--user",
        "--quiet",
        CALL_TIMEOUT,
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
        name,
        "fail",
        &count,
    ]
    .into_iter()
    .chain(properties.iter().flatten().copied())
    .chain(["0"])
    .map(str::to_owned)
    .collect()
}

/// Runs `busctl`; the error is its message.
fn busctl(args: &[String]) -> Result<(), String> {
    let out = Command::new("busctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("running busctl: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    Err(if message.is_empty() {
        format!("busctl {}", out.status)
    } else {
        message
    })
}

/// Waits until this process's cgroup is the scope `name`.
fn moved_into(name: &str) -> bool {
    let deadline = Instant::now() + MOVE_WAIT;
    loop {
        if own_cgroup().is_some_and(|cgroup| cgroup.rsplit('/').next() == Some(name)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unified_hierarchy_is_found_among_v1_lines() {
        let hybrid = "12:pids:/user.slice/user-1000.slice/user@1000.service\n\
                      1:name=systemd:/user.slice/user-1000.slice/user@1000.service/app.slice/a.service\n\
                      0::/user.slice/user-1000.slice/user@1000.service/app.slice/a.service\n";
        assert_eq!(
            unified(hybrid),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice/a.service")
        );
        assert_eq!(unified("1:name=systemd:/user.slice/a.scope\n"), None);
    }

    #[test]
    fn only_the_own_user_managers_tree_moves_and_keeps_its_slice() {
        let kde =
            "/user.slice/user-1000.slice/user@1000.service/app.slice/app-omp\\x2ddeck@1.service";
        assert_eq!(user_manager_slice(kde, 1000), Some("app.slice"));
        let nested = "/user.slice/user-1000.slice/user@1000.service/app.slice/\
                      app-graphical.slice/app-x-1.scope";
        assert_eq!(
            user_manager_slice(nested, 1000),
            Some("app-graphical.slice")
        );
        // Another user's manager, a login session (the system manager's) and a system service.
        for outside in [
            "/user.slice/user-10000.slice/user@10000.service/app.slice/a.service",
            "/user.slice/user-1000.slice/session-2.scope",
            "/system.slice/runner.service",
        ] {
            assert_eq!(user_manager_slice(outside, 1000), None, "{outside}");
        }
    }
}
