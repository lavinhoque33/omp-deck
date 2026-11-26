//! Processes a test sandbox leaves running. omp (19.x) starts a detached per-user daemon
//! (`omp __omp_worker_daemon`) once a session runs a PTY command or an eval worker; started under a
//! sandbox HOME it outlives the test (a minute or more), its directory deleted with the sandbox.
//! Also included by deck-core's live tests (`#[path]`); each includer uses part of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Calls [`stop`] for its root when dropped. Declare it after the sandbox's temp dir: locals drop
/// in reverse order, so the processes are stopped before the directory is removed.
pub struct Guard(pub PathBuf);

impl Drop for Guard {
    fn drop(&mut self) {
        stop(&self.0);
    }
}

/// Stops every process other than this one that still runs with its `HOME` inside `root`: SIGTERM
/// first (omp's daemon shuts itself down on it), SIGKILL for whatever is left after 2 s. Call it
/// when a sandbox is torn down, after its sessions are ended and before its files are removed.
/// Linux only: the environment of another process is read from `/proc`; the Windows runners of
/// the live CI tests are discarded after each job.
#[cfg(target_os = "linux")]
pub fn stop(root: &Path) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::time::{Duration, Instant};

    let me = std::process::id();
    let in_root = |pid: u32| {
        std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| {
            env.split(|&b| b == 0)
                .filter_map(|kv| kv.strip_prefix(b"HOME="))
                .any(|home| Path::new(OsStr::from_bytes(home)).starts_with(root))
        })
    };
    let mut strays: Vec<u32> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .filter(|&pid| pid != me && running(pid) && in_root(pid))
        .collect();
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        for &pid in &strays {
            // SAFETY: kill(2) has no memory-safety preconditions; `pid` is a positive process id
            // read from /proc (never 0 or -1, which would signal a whole group).
            unsafe { libc::kill(pid as libc::pid_t, signal) };
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !strays.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            strays.retain(|&pid| running(pid));
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn stop(_root: &Path) {}

/// Whether `pid` exists and has not exited: a zombie (an exited child of this test process that
/// nobody waited for) holds no files and counts as gone.
#[cfg(target_os = "linux")]
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // `pid (comm) state …`; comm may contain spaces and parentheses, the state follows the last ')'.
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .is_some_and(|state| !matches!(state, 'Z' | 'X'))
    })
}
