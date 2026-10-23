//! Named pipe endpoints: `\\.\pipe\omp-deck-host.<user>.<ns>.<name>`, `<user>` = 16 hex of
//! sha256(current user SID). DACL grants only the current user and SYSTEM, remote clients are
//! rejected, and the first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE` so a
//! second daemon for the same name fails.
//!
//! All pipe handles are overlapped: a blocking read on one thread and a write on another must
//! not serialize on the same file object.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FindClose, FindFirstFileW,
    FindNextFileW, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WIN32_FIND_DATAW, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
};

use crate::error::{Error, Result};
use crate::namespace::Namespace;
use crate::winproc::owned;

const PIPE_BUFFER: u32 = 64 * 1024;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}

/// Current user's SID as a string (`S-1-5-21-…`).
fn user_sid() -> Result<String> {
    let mut token = null_mut();
    // SAFETY: valid out pointer; the pseudo handle of the current process needs no closing.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(Error::last_os("OpenProcessToken"));
    }
    let token = owned(token, "OpenProcessToken")?;
    let mut len = 0u32;
    // SAFETY: size query with a null buffer; fails with ERROR_INSUFFICIENT_BUFFER by design.
    unsafe { GetTokenInformation(raw(&token), TokenUser, null_mut(), 0, &mut len) };
    // u64 elements keep the TOKEN_USER (pointer-aligned) buffer aligned.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: buf holds at least `len` bytes.
    if unsafe {
        GetTokenInformation(
            raw(&token),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    } == 0
    {
        return Err(Error::last_os("GetTokenInformation"));
    }
    // SAFETY: GetTokenInformation(TokenUser) filled a TOKEN_USER at the start of buf.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = null_mut();
    // SAFETY: sid points into buf (alive); text receives a LocalAlloc'ed string.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(Error::last_os("ConvertSidToStringSidW"));
    }
    // SAFETY: text is a NUL-terminated wide string from ConvertSidToStringSidW.
    let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    // SAFETY: len wide chars are readable.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: allocated by ConvertSidToStringSidW with LocalAlloc.
    unsafe { LocalFree(text.cast()) };
    Ok(s)
}

/// 16 hex chars of sha256(current user SID string).
pub(crate) fn user_key() -> Result<String> {
    let digest = Sha256::digest(user_sid()?.as_bytes());
    Ok(digest[..8].iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) struct Endpoint {
    name: String,
    pipe: Vec<u16>,
}

impl Endpoint {
    pub fn new(ns: &Namespace, name: &str) -> Result<Endpoint> {
        Ok(Endpoint {
            name: name.to_owned(),
            pipe: wide(&format!("{}{name}", ns.pipe_prefix())),
        })
    }
}

pub(crate) fn connect(endpoint: &Endpoint) -> Result<Stream> {
    for _ in 0..4 {
        // SAFETY: NUL-terminated name; no security attributes; no template.
        let handle = unsafe {
            CreateFileW(
                endpoint.pipe.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Stream::new(Arc::new(owned(handle, "CreateFileW")?), false);
        }
        // SAFETY: no preconditions.
        match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND => return Err(Error::NotRunning(endpoint.name.clone())),
            // Every instance is busy for a moment (the daemon creates the next one after each
            // connect): wait for one.
            // SAFETY: NUL-terminated name.
            ERROR_PIPE_BUSY => unsafe {
                WaitNamedPipeW(endpoint.pipe.as_ptr(), 1000);
            },
            _ => return Err(Error::last_os("CreateFileW(pipe)")),
        }
    }
    Err(Error::Timeout("waiting for a free pipe instance"))
}

/// Session names with a pipe in this namespace (liveness not checked).
pub(crate) fn list_names(ns: &Namespace) -> Result<Vec<String>> {
    let prefix = ns.pipe_prefix();
    let short_prefix = prefix.trim_start_matches(r"\\.\pipe\");
    let pattern = wide(r"\\.\pipe\*");
    // SAFETY: WIN32_FIND_DATAW is plain old data.
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated pattern and valid out struct.
    let find = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
    if find == INVALID_HANDLE_VALUE {
        // SAFETY: no preconditions.
        return match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND => Ok(Vec::new()),
            _ => Err(Error::last_os("FindFirstFileW(pipes)")),
        };
    }
    let mut names = Vec::new();
    loop {
        let len = data
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cFileName.len());
        let file = OsString::from_wide(&data.cFileName[..len]);
        if let Some(name) = file.to_str().and_then(|f| f.strip_prefix(short_prefix)) {
            names.push(name.to_owned());
        }
        // SAFETY: valid find handle and out struct.
        if unsafe { FindNextFileW(find, &mut data) } == 0 {
            break;
        }
    }
    // SAFETY: valid find handle, closed once.
    unsafe { FindClose(find) };
    Ok(names)
}

/// Security descriptor granting the current user and SYSTEM full access, nobody else.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is immutable after creation and only read by CreateNamedPipeW.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    fn current_user_only() -> Result<Self> {
        let sddl = wide(&format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", user_sid()?));
        let mut sd = null_mut();
        // SAFETY: NUL-terminated SDDL; sd receives a LocalAlloc'ed descriptor.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        } == 0
        {
            return Err(Error::last_os(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW",
            ));
        }
        Ok(SecurityDescriptor(sd))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

/// The daemon's pipe server. Exactly one instance listens at a time: a client connected to a
/// second listening instance would wait for an accept that is blocked on the first.
pub(crate) struct Listener {
    pipe: Vec<u16>,
    sd: SecurityDescriptor,
    /// The listening instance; `None` once removed.
    listening: Mutex<Option<Arc<OwnedHandle>>>,
    removed: AtomicBool,
    name: String,
}

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> Result<Listener> {
        let mut listener = Listener {
            pipe: endpoint.pipe.clone(),
            sd: SecurityDescriptor::current_user_only()?,
            listening: Mutex::new(None),
            removed: AtomicBool::new(false),
            name: endpoint.name.clone(),
        };
        let first = listener.instance(true)?;
        *listener
            .listening
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(first));
        Ok(listener)
    }

    fn instance(&self, first: bool) -> Result<OwnedHandle> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0,
            bInheritHandle: 0,
        };
        let open_mode = PIPE_ACCESS_DUPLEX
            | FILE_FLAG_OVERLAPPED
            | if first {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        // SAFETY: NUL-terminated name and a valid SECURITY_ATTRIBUTES for the call.
        let handle = unsafe {
            CreateNamedPipeW(
                self.pipe.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                &sa,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: no preconditions.
            if first && unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
                return Err(Error::AlreadyRunning(self.name.clone()));
            }
            return Err(Error::last_os("CreateNamedPipeW"));
        }
        owned(handle, "CreateNamedPipeW")
    }

    /// Waits for a client on the listening instance, then puts up the next one.
    pub fn accept(&self) -> io::Result<Stream> {
        let current = self
            .listening
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let pipe = match current {
            Some(p) => p,
            None if self.removed.load(Ordering::Acquire) => {
                // Removed: nothing will ever connect again; park this thread.
                loop {
                    std::thread::park();
                }
            }
            None => {
                let p = Arc::new(self.instance(false).map_err(io::Error::other)?);
                *self
                    .listening
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&p));
                p
            }
        };
        let connected = wait_connect(&pipe);
        // The next instance is created while this one still exists, so the pipe name never
        // disappears (clients would read that as "not running"); in the gap they see
        // ERROR_PIPE_BUSY and wait. After `remove` no new instance is created.
        {
            let mut listening = self
                .listening
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.removed.load(Ordering::Acquire) {
                *listening = None;
            } else {
                match self.instance(false) {
                    Ok(next) => *listening = Some(Arc::new(next)),
                    Err(e) => {
                        *listening = None;
                        log::warn!("creating the next pipe instance: {e}");
                    }
                }
            }
        }
        connected?;
        Stream::new(pipe, true).map_err(io::Error::other)
    }

    /// Stops listening: the endpoint disappears once every connected instance is closed.
    pub fn remove(&self) {
        self.removed.store(true, Ordering::Release);
        let listening = self
            .listening
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(pipe) = listening {
            // SAFETY: valid handle; cancels a pending ConnectNamedPipe in `accept`, which then
            // drops its reference and with it the last handle of this instance.
            unsafe { CancelIoEx(raw(&pipe), null()) };
        }
    }
}

/// Overlapped ConnectNamedPipe on a fresh instance, waited for.
fn wait_connect(pipe: &OwnedHandle) -> io::Result<()> {
    let event = new_event()?;
    // SAFETY: OVERLAPPED is plain old data.
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = raw(&event);
    // SAFETY: valid pipe handle; `ov` outlives the operation (waited for below).
    if unsafe { ConnectNamedPipe(raw(pipe), &mut ov) } != 0 {
        return Ok(());
    }
    // SAFETY: no preconditions.
    match unsafe { GetLastError() } {
        ERROR_PIPE_CONNECTED => Ok(()),
        ERROR_IO_PENDING => {
            let mut n = 0;
            // SAFETY: the pending operation's handle and OVERLAPPED; waits until it settles.
            if unsafe { GetOverlappedResult(raw(pipe), &ov, &mut n, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        // A client connected and already left.
        ERROR_NO_DATA => Err(io::ErrorKind::ConnectionAborted.into()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn new_event() -> io::Result<OwnedHandle> {
    // SAFETY: anonymous manual-reset event, initially non-signaled.
    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    owned(event, "CreateEventW").map_err(io::Error::other)
}

struct Pipe {
    handle: Arc<OwnedHandle>,
    server: bool,
    closed: AtomicBool,
}

/// A connected pipe; clones share the connection (each with its own wait events).
pub(crate) struct Stream {
    pipe: Arc<Pipe>,
    read_event: OwnedHandle,
    write_event: OwnedHandle,
    /// Read timeout in ms (`INFINITE` = none).
    timeout_ms: AtomicU32,
}

impl Stream {
    fn new(handle: Arc<OwnedHandle>, server: bool) -> Result<Stream> {
        Ok(Stream {
            pipe: Arc::new(Pipe {
                handle,
                server,
                closed: AtomicBool::new(false),
            }),
            read_event: new_event()?,
            write_event: new_event()?,
            timeout_ms: AtomicU32::new(INFINITE),
        })
    }

    pub fn try_clone(&self) -> io::Result<Stream> {
        Ok(Stream {
            pipe: Arc::clone(&self.pipe),
            read_event: new_event()?,
            write_event: new_event()?,
            timeout_ms: AtomicU32::new(INFINITE),
        })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        let ms = timeout.map_or(INFINITE, |t| {
            u32::try_from(t.as_millis()).unwrap_or(INFINITE - 1).max(1)
        });
        self.timeout_ms.store(ms, Ordering::Relaxed);
        Ok(())
    }

    /// Ends the connection. Server side: data already written is flushed to the client, then
    /// the pipe is disconnected (pending reads on clones fail). Client side: pending reads are
    /// cancelled and report end of stream.
    pub fn shutdown(&self) {
        let h = raw(&self.pipe.handle);
        self.pipe.closed.store(true, Ordering::Release);
        // SAFETY: valid handle owned by `self.pipe`.
        unsafe {
            if self.pipe.server {
                FlushFileBuffers(h);
                DisconnectNamedPipe(h);
            } else {
                CancelIoEx(h, null());
            }
        }
    }

    fn eof_error(code: u32) -> bool {
        matches!(
            code,
            ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA
        )
    }

    /// Runs one overlapped operation and waits for it (`timeout_ms` for reads).
    fn overlapped(
        &self,
        event: &OwnedHandle,
        timeout_ms: u32,
        start: impl FnOnce(*mut OVERLAPPED) -> i32,
    ) -> io::Result<usize> {
        let h = raw(&self.pipe.handle);
        // SAFETY: OVERLAPPED is plain old data.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = raw(event);
        if start(&mut ov) == 0 {
            // SAFETY: no preconditions.
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING {
                if Self::eof_error(code) {
                    return Ok(0);
                }
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            // SAFETY: valid event handle.
            let waited = unsafe { WaitForSingleObject(raw(event), timeout_ms) };
            if waited == WAIT_TIMEOUT {
                // SAFETY: cancels exactly this operation; then wait for it to settle so `ov`
                // and the buffer are no longer referenced.
                unsafe { CancelIoEx(h, &ov) };
            } else if waited != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut n = 0u32;
        // SAFETY: the operation's handle and OVERLAPPED; bWait = TRUE blocks until settled.
        if unsafe { GetOverlappedResult(h, &ov, &mut n, 1) } == 0 {
            // SAFETY: no preconditions.
            let code = unsafe { GetLastError() };
            if Self::eof_error(code) {
                return Ok(0);
            }
            if code == ERROR_OPERATION_ABORTED {
                if self.pipe.closed.load(Ordering::Acquire) {
                    return Ok(0);
                }
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        Ok(n as usize)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pipe.closed.load(Ordering::Acquire) {
            return Ok(0);
        }
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let h = raw(&self.pipe.handle);
        let timeout = self.timeout_ms.load(Ordering::Relaxed);
        let ptr = buf.as_mut_ptr();
        // SAFETY: buf is valid for `len` bytes until `overlapped` returns (it waits for
        // completion or cancellation).
        self.overlapped(&self.read_event, timeout, |ov| unsafe {
            ReadFile(h, ptr, len, null_mut(), ov)
        })
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pipe.closed.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let h = raw(&self.pipe.handle);
        let ptr = buf.as_ptr();
        // SAFETY: buf is valid for `len` bytes until `overlapped` returns.
        let n = self.overlapped(&self.write_event, INFINITE, |ov| unsafe {
            WriteFile(h, ptr, len, null_mut(), ov)
        })?;
        if n == 0 && !buf.is_empty() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
