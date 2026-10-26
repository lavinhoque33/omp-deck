//! Windows side of [`super`].

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::ptr;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, OpenThread,
    PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, THREAD_SUSPEND_RESUME,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;

use super::FileId;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `SW_SHOWNORMAL`.
const SHOW_NORMAL: i32 = 1;
/// 100 ns ticks between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

pub fn root_dir() -> PathBuf {
    let drive = std::env::var_os("SystemDrive")
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| OsString::from("C:"));
    let mut root = PathBuf::from(drive);
    root.push(r"\");
    root
}

pub fn hostname_os() -> Option<String> {
    let mut len = 0u32;
    // SAFETY: a null buffer with `len` = 0 only asks for the required size (incl. the NUL).
    unsafe { GetComputerNameExW(ComputerNameDnsHostname, ptr::null_mut(), &mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` UTF-16 units; the call writes at most that many.
    let ok = unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut len) };
    // On success `len` is the length without the NUL.
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
}

pub fn process_start_os(pid: u32) -> Option<SystemTime> {
    // SAFETY: plain call; a null result means failure.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` is a handle we own; `OwnedHandle` closes it.
    let process = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the handle is valid and the four out-pointers point to live FILETIMEs.
    let ok = unsafe {
        GetProcessTimes(
            process.as_raw_handle() as HANDLE,
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    let since_unix = ticks.checked_sub(FILETIME_UNIX_EPOCH)?;
    Some(UNIX_EPOCH + Duration::from_nanos(since_unix.saturating_mul(100)))
}

/// ppid → children of every process, from a Toolhelp32 snapshot (empty when it fails).
pub fn process_children() -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    // SAFETY: plain snapshot of all processes; the handle is owned and closed by `OwnedHandle`.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return map;
    }
    // SAFETY: we own the snapshot handle.
    let _snap = unsafe { OwnedHandle::from_raw_handle(snap as RawHandle) };
    // SAFETY: plain data; all-zero is a valid value before `dwSize` is set.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `entry.dwSize` is set and `snap` is a process snapshot.
    let mut more = unsafe { Process32FirstW(snap, &mut entry) } != 0;
    while more {
        map.entry(entry.th32ParentProcessID)
            .or_default()
            .push(entry.th32ProcessID);
        // SAFETY: as for `Process32FirstW`.
        more = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    map
}

/// Private bytes of `pid` (`PrivateUsage`: committed memory it does not share), else its working
/// set; `None` when the process is gone or cannot be opened.
pub fn process_memory(pid: u32) -> Option<u64> {
    // SAFETY: plain call; a null result means failure.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` is a handle we own; `OwnedHandle` closes it.
    let process = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
    // SAFETY: plain data; all-zero is a valid value.
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    counters.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: the handle is valid; `PROCESS_MEMORY_COUNTERS_EX` starts with
    // `PROCESS_MEMORY_COUNTERS` and `cb` tells the API the larger size.
    let ok = unsafe {
        GetProcessMemoryInfo(
            process.as_raw_handle() as HANDLE,
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        return None;
    }
    let bytes = if counters.PrivateUsage != 0 {
        counters.PrivateUsage
    } else {
        counters.WorkingSetSize
    };
    Some(bytes as u64)
}

/// 16 hex digits of sha256 over the current user's SID bytes: names per-user endpoints (named
/// pipes are machine-global). Falls back to the user name when the token cannot be read.
pub fn user_tag() -> &'static str {
    static TAG: LazyLock<String> = LazyLock::new(|| {
        let id = current_sid().unwrap_or_else(|| {
            std::env::var_os("USERNAME")
                .map(|u| u.to_string_lossy().into_owned().into_bytes())
                .unwrap_or_default()
        });
        Sha256::digest(&id)[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    });
    &TAG
}

/// The raw SID of the current user (for the gateway's scheduled-task principal).
pub fn user_sid_bytes() -> Option<Vec<u8>> {
    current_sid()
}

fn current_sid() -> Option<Vec<u8>> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo-handle of the current process is always valid; `token` is an out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    // SAFETY: the token handle is ours; `OwnedHandle` closes it.
    let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };
    let handle = token.as_raw_handle() as HANDLE;
    let mut needed = 0u32;
    // SAFETY: a null buffer only queries the required size.
    unsafe { GetTokenInformation(handle, TokenUser, ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    // u64 backing keeps the TOKEN_USER header 8-byte aligned.
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` has at least `needed` bytes.
    let ok = unsafe {
        GetTokenInformation(
            handle,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points into the buffer.
    let (sid, len) = unsafe {
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        (user.User.Sid, GetLengthSid(user.User.Sid) as usize)
    };
    // SAFETY: `sid` is valid for `len` bytes (GetLengthSid) while `buf` lives.
    Some(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), len) }.to_vec())
}

/// Open-file introspection of other processes is not available: no files are reported.
pub fn tree_open_files(roots: &[u32]) -> Vec<Vec<PathBuf>> {
    roots.iter().map(|_| Vec::new()).collect()
}

pub fn no_window(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NO_WINDOW)
}

/// `PATHEXT` entries, lowercased with their dot; the system default when unset or empty.
pub fn path_extensions() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_default();
    let mut exts: Vec<String> = raw
        .split(';')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| e.len() > 1 && e.starts_with('.'))
        .collect();
    if exts.is_empty() {
        exts = [".com", ".exe", ".bat", ".cmd"].map(str::to_owned).to_vec();
    }
    exts
}

pub fn executable(path: &Path, _meta: &Metadata) -> bool {
    let Some(ext) = path.extension().and_then(OsStr::to_str) else {
        return false;
    };
    let dotted = format!(".{}", ext.to_ascii_lowercase());
    path_extensions().contains(&dotted)
}

pub fn private_dir_builder(recursive: bool) -> DirBuilder {
    let mut b = DirBuilder::new();
    b.recursive(recursive);
    b
}

/// Windows files inherit the ACL of their (user-profile) parent: nothing to set.
pub fn private_file_options(_o: &mut OpenOptions) {}

/// No POSIX modes: archives record 0o755 for files Windows would run, 0o644 for the rest.
pub fn file_mode(path: &Path, _meta: &Metadata) -> u32 {
    let runs = path.extension().and_then(OsStr::to_str).is_some_and(|e| {
        ["exe", "bat", "cmd", "com", "ps1"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    });
    if runs { 0o755 } else { 0o644 }
}

pub fn set_file_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

pub fn file_identity(file: &File) -> io::Result<FileId> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data; all-zero is a valid value.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle belongs to `file`; `info` is a valid out-pointer.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileId(
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Opens a regular file for reading without following a symlink or junction: the link itself is
/// opened (`FILE_FLAG_OPEN_REPARSE_POINT`) and rejected because it is not a regular file.
pub fn open_regular_file(path: &Path) -> io::Result<File> {
    let not_regular = || io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(not_regular());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

/// Job object that kills everything inside when terminated or when its last handle closes.
pub struct Tree {
    job: OwnedHandle,
}

const CREATE_SUSPENDED: u32 = 0x0000_0004;

/// Starts `cmd` suspended, puts it into a fresh kill-on-close job, then lets it run: nothing it
/// starts can be outside the job. If the job cannot be set up or joined the child is killed and
/// the spawn fails.
pub fn spawn(cmd: &mut Command) -> io::Result<(Child, Tree)> {
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = cmd.spawn()?;
    let started = new_job(&child).and_then(|job| resume(child.id()).map(|()| job));
    match started {
        Ok(job) => Ok((child, Tree { job })),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

/// Resumes the (single) initial thread of the suspended process `pid`: std does not hand out the
/// thread handle, so it is found through a thread snapshot.
fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: plain snapshot of all threads; the handle is owned and closed by `OwnedHandle`.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: we own the snapshot handle.
    let _snap = unsafe { OwnedHandle::from_raw_handle(snap as RawHandle) };
    // SAFETY: plain data; all-zero is a valid value before `dwSize` is set.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut resumed = false;
    // SAFETY: `entry.dwSize` is set and `snap` is a thread snapshot.
    let mut more = unsafe { Thread32First(snap, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opens a thread by id for suspend/resume only; closed below.
            let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: we own the new handle.
            let thread = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
            // SAFETY: the handle is valid and has THREAD_SUSPEND_RESUME.
            if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed = true;
        }
        // SAFETY: as for `Thread32First`.
        more = unsafe { Thread32Next(snap, &mut entry) } != 0;
    }
    if resumed {
        Ok(())
    } else {
        Err(io::Error::other(
            "the suspended child has no thread to resume",
        ))
    }
}

impl Tree {
    pub fn kill(&self) {
        // SAFETY: the job handle is valid while `self` lives.
        unsafe { TerminateJobObject(self.job.as_raw_handle() as HANDLE, 1) };
    }
}

fn new_job(child: &Child) -> io::Result<OwnedHandle> {
    // SAFETY: null attributes and name create an anonymous job.
    let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: we own the new handle; `OwnedHandle` closes it.
    let job = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
    // SAFETY: plain data; all-zero is a valid value.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `limits` matches the information class and the length passed.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            ptr::from_ref(&limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both handles are valid; nested jobs are allowed since Windows 8.
    let ok = unsafe {
        AssignProcessToJobObject(
            job.as_raw_handle() as HANDLE,
            child.as_raw_handle() as HANDLE,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// `ShellExecuteW(open)`: succeeds when the shell accepted the request (result > 32).
pub fn open_path_os(target: &OsStr) -> io::Result<()> {
    let verb = wide(OsStr::new("open"));
    let file = wide(target);
    // SAFETY: both strings are NUL-terminated and outlive the call; null window/params/dir.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SHOW_NORMAL,
        )
    };
    if result as usize > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ShellExecute failed (code {})",
            result as usize
        )))
    }
}

/// `explorer.exe /select,"<file>"`. Explorer exits non-zero even on success, so only a failed
/// start is an error.
pub fn reveal_os(file: &Path) -> io::Result<()> {
    let path = file.to_string_lossy().replace('/', "\\");
    Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{path}\""))
        .spawn()?
        .wait()?;
    Ok(())
}

/// What keeps a [`super::NamedLock`] held: the handle of the named mutex.
pub type Held = OwnedHandle;

pub fn try_named_lock(name: &str, _path: &Path) -> io::Result<Option<Held>> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, SetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let wide: Vec<u16> = format!(r"Global\{name}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: resets the calling thread's last-error value so the read below is this call's.
    unsafe { SetLastError(0) };
    // SAFETY: the attributes pointer is null and `wide` is a live NUL-terminated UTF-16 string.
    let raw = unsafe { CreateMutexW(ptr::null(), 0, wide.as_ptr()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `CreateMutexW` returned a fresh handle that only this value owns; it is closed
    // exactly once, on every path (a losing contender drops it right away).
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: reads the last-error value `CreateMutexW` just set; nothing ran in between.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(None);
    }
    Ok(Some(handle))
}
