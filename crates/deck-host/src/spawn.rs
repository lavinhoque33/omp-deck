//! Client side of daemon startup: the private spec file and the detached daemon process.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

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

/// Handle of the launched daemon process (Unix: the launcher that exits once the forked
/// daemon serves).
#[cfg(unix)]
pub(crate) type Launcher = std::process::Child;

/// Starts `exe serve --ns <ns> --name <name> --spec <spec>` detached from the caller.
/// Unix: the daemon forks into its own session itself (the launched process exits once the
/// daemon serves).
#[cfg(unix)]
pub(crate) fn spawn_daemon(exe: &Path, ns: &str, name: &str, spec: &Path) -> io::Result<Launcher> {
    use std::process::{Command, Stdio};
    Command::new(exe)
        .arg("serve")
        .arg("--ns")
        .arg(ns)
        .arg("--name")
        .arg(name)
        .arg("--spec")
        .arg(spec)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// The daemon process itself (Windows).
#[cfg(windows)]
pub(crate) struct Launcher(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl Launcher {
    /// Exit status if the daemon has exited.
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::ExitStatusExt;
        use windows_sys::Win32::Foundation::{STILL_ACTIVE, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        let h = self.0.as_raw_handle();
        // SAFETY: valid process handle; zero timeout.
        if unsafe { WaitForSingleObject(h, 0) } != WAIT_OBJECT_0 {
            return Ok(None);
        }
        let mut code = STILL_ACTIVE as u32;
        // SAFETY: valid process handle and out pointer.
        if unsafe { GetExitCodeProcess(h, &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(std::process::ExitStatus::from_raw(code)))
    }
}

/// Starts `exe serve --ns <ns> --name <name> --spec <spec>` detached (own process group, no
/// console, out of the caller's job when allowed).
///
/// Not `std::process::Command`: it creates the child with `bInheritHandles = TRUE`, which
/// hands the daemon every inheritable handle of the caller (e.g. the stdout pipe of a CLI run
/// under a test harness, or a GUI helper's pipes), pinning those pipes open for the daemon's
/// whole life. Here only a `NUL` handle for stdio is inherited, via
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
#[cfg(windows)]
pub(crate) fn spawn_daemon(exe: &Path, ns: &str, name: &str, spec: &Path) -> io::Result<Launcher> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateProcessW,
        DETACHED_PROCESS, EXTENDED_STARTUPINFO_PRESENT, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    use crate::pty::winargs::quote_arg;
    use crate::winproc::{AttributeList, io_error, owned};

    let mut line = String::new();
    quote_arg(&exe.to_string_lossy(), false, &mut line);
    for arg in [
        "serve",
        "--ns",
        ns,
        "--name",
        name,
        "--spec",
        &spec.to_string_lossy(),
    ] {
        line.push(' ');
        quote_arg(arg, false, &mut line);
    }
    let mut line: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    let app: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Inheritable, but only ever handed to this child through the handle list.
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let nul: Vec<u16> = "NUL".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated device name and valid security attributes.
    let nul: OwnedHandle = owned(
        unsafe {
            CreateFileW(
                nul.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa,
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        },
        "CreateFileW(NUL)",
    )
    .map_err(io_error)?;
    let handles: [HANDLE; 1] = [nul.as_raw_handle()];
    let mut attrs = AttributeList::new(1).map_err(io_error)?;
    // SAFETY: `handles` lives until after CreateProcessW below.
    unsafe {
        attrs.set(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handles.as_ptr().cast(),
            std::mem::size_of_val(&handles),
        )
    }
    .map_err(io_error)?;

    // SAFETY: STARTUPINFOEXW is plain old data.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = handles[0];
    si.StartupInfo.hStdOutput = handles[0];
    si.StartupInfo.hStdError = handles[0];
    si.lpAttributeList = attrs.as_ptr();

    let base = DETACHED_PROCESS
        | CREATE_NEW_PROCESS_GROUP
        | CREATE_NO_WINDOW
        | EXTENDED_STARTUPINFO_PRESENT;
    let mut create = |flags: u32| {
        // SAFETY: plain old data out struct.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: NUL-terminated application name and mutable command line; startup info
        // with an initialized attribute list whose handle list holds only `nul`; the
        // caller's environment and directory.
        let ok = unsafe {
            CreateProcessW(
                app.as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                null(),
                null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // The thread handle is not needed.
        drop(owned(pi.hThread, "CreateProcessW").map_err(io_error)?);
        owned(pi.hProcess, "CreateProcessW")
            .map(Launcher)
            .map_err(io_error)
    };
    match create(base | CREATE_BREAKAWAY_FROM_JOB) {
        // The caller's job forbids breakaway: start inside it.
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => create(base),
        other => other,
    }
}
