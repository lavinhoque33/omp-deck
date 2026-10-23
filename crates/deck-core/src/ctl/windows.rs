//! Windows endpoint of the ctl channel: a named pipe the extension serves with `node:net`
//! (`\\.\pipe\ompdeck-ctl.<user>.<namespace>.<leaf>`). There is nothing to create, lock or prune:
//! the pipe exists exactly as long as the omp process that listens on it, and an unclaimed name
//! simply fails to connect.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, HANDLE};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

use super::{POLL, auth_line, load_or_create_secret, pipe_name, pipe_token};
use crate::platform;

/// The extension picks its endpoint from this variable (the full pipe name; it never derives one).
pub(super) const ENV_VAR: &str = "OMPDECK_CTL_PIPE";
/// How often [`Conn::read`] looks for data while it waits.
const PEEK_EVERY: Duration = Duration::from_millis(5);
const PIPE_PREFIX: &str = r"\\.\pipe\ompdeck-ctl.";

/// The pipe name for a tab (a path-typed value, so callers treat both OSes alike).
pub fn ctl_dir(namespace: &str, key: &str) -> PathBuf {
    PathBuf::from(pipe_name(platform::user_tag(), namespace, key))
}

/// Nothing to prepare for a pipe; only a name that is not one of ours is refused.
pub fn prepare_ctl_dir(dir: &Path) -> io::Result<()> {
    if dir.to_str().is_some_and(|d| d.starts_with(PIPE_PREFIX)) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an OmpDeck ctl pipe name", dir.display()),
        ))
    }
}

/// Pipes disappear with their server: nothing is left over to prune.
pub fn prune_ctl_dirs(_namespace: &str, _alive: &HashSet<String>) {}

/// An installed extension is current when it is a regular file (no link) with this content.
pub(super) fn asset_is_current(path: &Path, source: &str) -> io::Result<bool> {
    Ok(
        fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
            && fs::read(path).is_ok_and(|b| b == source.as_bytes()),
    )
}

/// Windows has no mode bits to tighten; the file inherits its directory's ACL.
pub(super) fn seal_asset(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// The client end of the pipe. A synchronous read on a pipe handle would block every other call
/// on the same handle, so [`Read::read`] polls `PeekNamedPipe` instead and reports
/// [`io::ErrorKind::TimedOut`] after [`POLL`] without data, like a socket with a read timeout.
pub(super) struct Conn {
    pipe: File,
}

const TOKEN_VAR: &str = "OMPDECK_CTL_TOKEN";

/// The token of the pipe named `dir` (see [`pipe_token`]).
fn token_for(dir: &Path, key_file: &Path) -> io::Result<String> {
    let name = dir
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pipe name is not UTF-8"))?;
    Ok(pipe_token(&load_or_create_secret(key_file)?, name))
}

/// The extension's environment: the pipe name and the token that authenticates clients of it.
pub(super) fn env(dir: &Path, key_file: &Path) -> io::Result<Vec<(&'static str, OsString)>> {
    Ok(vec![
        (ENV_VAR, dir.as_os_str().to_owned()),
        (TOKEN_VAR, OsString::from(token_for(dir, key_file)?)),
    ])
}

/// Opens the pipe `dir` names and authenticates (the extension ignores anything before the
/// token). A missing pipe is `NotFound`.
pub(super) fn connect(dir: &Path, key_file: &Path) -> io::Result<Conn> {
    let mut pipe = OpenOptions::new().read(true).write(true).open(dir)?;
    pipe.write_all(&auth_line(&token_for(dir, key_file)?))?;
    Ok(Conn { pipe })
}

impl Conn {
    /// Bytes ready to read; `None` once the server closed its end.
    fn available(&self) -> io::Result<Option<usize>> {
        let mut avail = 0u32;
        // SAFETY: the handle is the pipe's and stays open while `self` lives; only the
        // total-available out-pointer is given, the others are allowed to be null.
        let ok = unsafe {
            PeekNamedPipe(
                self.pipe.as_raw_handle() as HANDLE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut avail,
                ptr::null_mut(),
            )
        };
        if ok != 0 {
            return Ok(Some(avail as usize));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error().map(|c| c as u32) {
            Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED) => Ok(None),
            _ => Err(err),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let deadline = Instant::now() + POLL;
        loop {
            match self.available()? {
                None => return Ok(0),
                Some(n) if n > 0 => {
                    let take = n.min(buf.len());
                    return self.pipe.read(&mut buf[..take]);
                }
                Some(_) => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            thread::sleep(PEEK_EVERY.min(left));
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pipe.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()
    }
}
