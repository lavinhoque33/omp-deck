//! Client side of daemon startup: the private spec file and the detached daemon process.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::error::Result;
use crate::namespace::Namespace;

/// Writes the spawn request to a new owner-only file the daemon reads and deletes.
/// Unix: inside the 0700 namespace runtime directory (created/verified here). Windows: the
/// per-user temp directory.
pub(crate) fn write_spec(ns: &Namespace, name: &str, contents: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    let dir = {
        let endpoint = ns.endpoint(name)?;
        crate::transport::prepare(&endpoint)?;
        ns.dir()
    };
    #[cfg(windows)]
    let dir = {
        let _ = ns;
        std::env::temp_dir()
    };
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
    let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("omp-deck-host.{name}.{hex}.spec"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    if let Err(e) = file.write_all(contents).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&path);
        return Err(e.into());
    }
    Ok(path)
}

/// Starts `exe serve --ns <ns> --name <name> --spec <spec>` detached from the caller.
/// Unix: the daemon forks into its own session itself (the launched process exits once the
/// daemon serves). Windows: detached, own process group, no window, out of the caller's job
/// when allowed.
pub(crate) fn spawn_daemon(exe: &Path, ns: &str, name: &str, spec: &Path) -> io::Result<Child> {
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--ns")
        .arg(ns)
        .arg("--name")
        .arg(name)
        .arg("--spec")
        .arg(spec)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        use windows_sys::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
        };
        let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
        cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            // The caller's job forbids breakaway: start inside it.
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                cmd.creation_flags(base);
                cmd.spawn()
            }
            other => other,
        }
    }
    #[cfg(unix)]
    cmd.spawn()
}
