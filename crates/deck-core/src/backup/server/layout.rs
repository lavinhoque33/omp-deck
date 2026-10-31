//! How backups are kept in the server folder, over any [`Transport`]: archive `<name>` plus
//! sidecar `<name>.json` (`{created, size, props}`); [`BackupFile::id`] is the archive name. Both
//! are uploaded as `<file>.part` and renamed, the sidecar last: a backup without one is
//! incomplete and never listed. Deleting removes the sidecar first.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::backup::remote::{
    BackupFile, MAX_DOWNLOAD, Remote, is_config_backup, is_project_backup,
};
use crate::backup::{BACKUP_PREFIX, Error, PROJECT_PREFIX, Result, rfc3339, unix_now};
use crate::usage::parse_iso_ms;

/// Sidecars are small; anything bigger is not ours.
const MAX_SIDECAR: u64 = 64 << 10;
const SIDECAR: &str = ".json";
const PART: &str = ".part";
/// Progress is reported at most once per this many bytes.
const REPORT_STEP: u64 = 1 << 20;

/// File operations in the server folder; names are plain file names (validated by the caller).
pub(super) trait Transport {
    /// Every file name in the folder.
    fn list(&mut self) -> Result<Vec<String>>;
    /// Streams `name` into `out`.
    fn read(&mut self, name: &str, out: &mut dyn Write) -> Result<()>;
    /// Creates or replaces `name` with everything `data` yields.
    fn write(&mut self, name: &str, data: &mut dyn Read) -> Result<()>;
    fn rename(&mut self, from: &str, to: &str) -> Result<()>;
    fn remove(&mut self, name: &str) -> Result<()>;
}

/// A server backup's metadata file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::backup) struct Sidecar {
    /// RFC 3339.
    pub(in crate::backup) created: String,
    pub(in crate::backup) size: u64,
    /// The same keys as Drive appProperties.
    #[serde(default)]
    pub(in crate::backup) props: BTreeMap<String, String>,
}

impl Sidecar {
    /// `None` when `bytes` is not a sidecar with a readable creation time.
    pub(in crate::backup) fn parse(bytes: &[u8]) -> Option<Sidecar> {
        let sidecar: Sidecar = serde_json::from_slice(bytes).ok()?;
        parse_iso_ms(&sidecar.created).map(|_| sidecar)
    }

    fn file(self, name: &str) -> BackupFile {
        BackupFile {
            id: name.to_owned(),
            name: name.to_owned(),
            size: self.size,
            created: self.created,
            props: self.props,
        }
    }
}

/// A file or folder name that can be put into a remote path as is: no separators, no `..`, no
/// control characters, at most 255 bytes.
pub(in crate::backup) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains("..")
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
}

/// A backup archive name of this app: what every id handed to the server must be.
pub(in crate::backup) fn valid_id(id: &str) -> bool {
    valid_name(id)
        && (id.starts_with(BACKUP_PREFIX) || id.starts_with(PROJECT_PREFIX))
        && id.ends_with(".tar.gz")
}

fn check_id(id: &str) -> Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(Error::Server(format!("invalid backup name “{id}”")))
    }
}

fn gone() -> Error {
    Error::Server("the backup no longer exists on the server".to_owned())
}

/// A connected server folder.
pub(in crate::backup) struct ServerRemote {
    conn: Box<dyn Transport>,
}

impl ServerRemote {
    pub(super) fn new(conn: Box<dyn Transport>) -> ServerRemote {
        ServerRemote { conn }
    }

    /// Proves write access: writes a probe as `.part`, renames it and deletes it.
    pub(super) fn probe(&mut self) -> Result<()> {
        let mut tag = [0u8; 6];
        getrandom::fill(&mut tag).map_err(|e| Error::Io(format!("no randomness: {e}")))?;
        let name = format!(
            ".omp-deck-probe-{}",
            tag.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let part = format!("{name}{PART}");
        let step = |what: &str, r: Result<()>| {
            r.map_err(|e| Error::Server(format!("the folder is not writable ({what}): {e}")))
        };
        step("upload", self.conn.write(&part, &mut &b"omp-deck"[..]))?;
        if let Err(e) = step("rename", self.conn.rename(&part, &name)) {
            let _ = self.conn.remove(&part);
            return Err(e);
        }
        step("delete", self.conn.remove(&name))
    }

    /// Every complete backup whose name starts with `prefix`.
    fn list_prefixed(&mut self, prefix: &str) -> Result<Vec<BackupFile>> {
        let names: BTreeSet<String> = self.conn.list()?.into_iter().collect();
        let mut out = Vec::new();
        for name in &names {
            if !name.starts_with(prefix)
                || !valid_id(name)
                || !names.contains(&format!("{name}{SIDECAR}"))
            {
                continue;
            }
            match self.read_sidecar(name)? {
                Some(s) => out.push(s.file(name)),
                None => log::warn!("{name}: unreadable sidecar; not listed"),
            }
        }
        Ok(out)
    }

    fn read_sidecar(&mut self, name: &str) -> Result<Option<Sidecar>> {
        let mut out = Capped::new(MAX_SIDECAR);
        match self.conn.read(&format!("{name}{SIDECAR}"), &mut out) {
            Ok(()) => Ok(Sidecar::parse(&out.buf)),
            Err(_) if out.over => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn exists(&mut self, name: &str) -> Result<bool> {
        Ok(self.conn.list()?.iter().any(|n| n == name))
    }

    /// `<name>.part` → `<name>`, then the sidecar the same way. A failed or cancelled upload
    /// removes its `.part` (best effort).
    fn upload(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        data: &mut dyn Read,
        total: u64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<BackupFile> {
        check_id(name)?;
        let part = format!("{name}{PART}");
        let mut meter = Meter::new(data, cancel, progress);
        let sent = self.conn.write(&part, &mut meter);
        let (done, cancelled) = (meter.done, meter.cancelled);
        if let Err(e) = sent {
            let _ = self.conn.remove(&part);
            return Err(if cancelled { Error::Cancelled } else { e });
        }
        if done != total {
            let _ = self.conn.remove(&part);
            return Err(Error::Server(format!(
                "Uploading the backup failed: sent {done} bytes instead of {total}"
            )));
        }
        self.conn.rename(&part, name)?;
        let sidecar = Sidecar {
            created: rfc3339(unix_now()),
            size: total,
            props: props.clone(),
        };
        let json = serde_json::to_vec_pretty(&sidecar).map_err(|e| Error::Io(e.to_string()))?;
        let side = format!("{name}{SIDECAR}");
        let side_part = format!("{side}{PART}");
        if let Err(e) = self
            .conn
            .write(&side_part, &mut &json[..])
            .and_then(|()| self.conn.rename(&side_part, &side))
        {
            let _ = self.conn.remove(&side_part);
            let _ = self.conn.remove(name);
            return Err(e);
        }
        Ok(sidecar.file(name))
    }

    /// Sidecar first (the backup disappears from listings at once), then the archive; files
    /// that are already gone are skipped.
    fn remove_backup(&mut self, id: &str) -> Result<()> {
        check_id(id)?;
        let names: BTreeSet<String> = self.conn.list()?.into_iter().collect();
        let side = format!("{id}{SIDECAR}");
        if names.contains(&side) {
            self.conn.remove(&side)?;
        }
        if names.contains(id) {
            self.conn.remove(id)?;
        }
        Ok(())
    }
}

impl Remote for ServerRemote {
    /// [`super::open`] already connected and logged in.
    fn check(&mut self) -> Result<()> {
        Ok(())
    }

    fn list_config(&mut self) -> Result<Vec<BackupFile>> {
        let mut files = self.list_prefixed(BACKUP_PREFIX)?;
        files.retain(is_config_backup);
        Ok(files)
    }

    fn list_projects(&mut self) -> Result<Vec<BackupFile>> {
        let mut files = self.list_prefixed(PROJECT_PREFIX)?;
        files.retain(is_project_backup);
        Ok(files)
    }

    fn upload_bytes(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        bytes: &[u8],
    ) -> Result<BackupFile> {
        let never = AtomicBool::new(false);
        self.upload(
            name,
            props,
            &mut &bytes[..],
            bytes.len() as u64,
            &never,
            &mut |_| {},
        )
    }

    fn upload_file(
        &mut self,
        name: &str,
        props: &BTreeMap<String, String>,
        file: &mut File,
        total: u64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<BackupFile> {
        self.upload(name, props, &mut file.take(total), total, cancel, progress)
    }

    fn download(&mut self, id: &str) -> Result<Vec<u8>> {
        check_id(id)?;
        if !self.exists(id)? {
            return Err(gone());
        }
        let mut out = Capped::new(MAX_DOWNLOAD);
        match self.conn.read(id, &mut out) {
            Ok(()) => Ok(out.buf),
            Err(_) if out.over => Err(Error::Server(format!(
                "Downloading the backup failed: it is larger than {} MiB",
                MAX_DOWNLOAD >> 20
            ))),
            Err(e) => Err(e),
        }
    }

    fn download_to(
        &mut self,
        id: &str,
        size: u64,
        out: &mut File,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        check_id(id)?;
        if !self.exists(id)? {
            return Err(gone());
        }
        let mut sink = MeterOut::new(out, cancel, progress);
        let got = self.conn.read(id, &mut sink);
        let (done, cancelled) = (sink.done, sink.cancelled);
        if let Err(e) = got {
            return Err(if cancelled { Error::Cancelled } else { e });
        }
        if done != size {
            return Err(Error::Server(format!(
                "Downloading the backup failed: got {done} of {size} bytes"
            )));
        }
        Ok(())
    }

    fn metadata(&mut self, id: &str) -> Result<Option<BackupFile>> {
        check_id(id)?;
        let names: BTreeSet<String> = self.conn.list()?.into_iter().collect();
        if !names.contains(id) || !names.contains(&format!("{id}{SIDECAR}")) {
            return Ok(None);
        }
        match self.read_sidecar(id)? {
            Some(s) => Ok(Some(s.file(id))),
            None => Err(Error::Server(format!(
                "the details of {id} on the server are unreadable"
            ))),
        }
    }

    fn delete(&mut self, id: &str) -> Result<()> {
        self.remove_backup(id)
    }

    fn delete_permanently(&mut self, id: &str) -> Result<()> {
        self.remove_backup(id)
    }
}

/// The I/O error that carries a cancel out of a transport.
fn cancelled_io() -> io::Error {
    io::Error::other("cancelled")
}

/// Reads through to `inner`, counting, reporting progress and honouring cancel.
struct Meter<'a> {
    inner: &'a mut dyn Read,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(u64),
    done: u64,
    reported: u64,
    cancelled: bool,
}

impl<'a> Meter<'a> {
    fn new(
        inner: &'a mut dyn Read,
        cancel: &'a AtomicBool,
        progress: &'a mut dyn FnMut(u64),
    ) -> Meter<'a> {
        Meter {
            inner,
            cancel,
            progress,
            done: 0,
            reported: 0,
            cancelled: false,
        }
    }
}

impl Read for Meter<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            self.cancelled = true;
            return Err(cancelled_io());
        }
        let n = self.inner.read(buf)?;
        self.done += n as u64;
        if n == 0 || self.done - self.reported >= REPORT_STEP {
            self.reported = self.done;
            (self.progress)(self.done);
        }
        Ok(n)
    }
}

/// Writes through to `inner`, counting, reporting progress and honouring cancel.
struct MeterOut<'a> {
    inner: &'a mut dyn Write,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(u64),
    done: u64,
    reported: u64,
    cancelled: bool,
}

impl<'a> MeterOut<'a> {
    fn new(
        inner: &'a mut dyn Write,
        cancel: &'a AtomicBool,
        progress: &'a mut dyn FnMut(u64),
    ) -> MeterOut<'a> {
        MeterOut {
            inner,
            cancel,
            progress,
            done: 0,
            reported: 0,
            cancelled: false,
        }
    }
}

impl Write for MeterOut<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            self.cancelled = true;
            return Err(cancelled_io());
        }
        let n = self.inner.write(buf)?;
        self.done += n as u64;
        if self.done - self.reported >= REPORT_STEP {
            self.reported = self.done;
            (self.progress)(self.done);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        if self.done != self.reported {
            self.reported = self.done;
            (self.progress)(self.done);
        }
        Ok(())
    }
}

/// An in-memory sink that refuses more than `cap` bytes.
struct Capped {
    buf: Vec<u8>,
    cap: u64,
    over: bool,
}

impl Capped {
    fn new(cap: u64) -> Capped {
        Capped {
            buf: Vec::new(),
            cap,
            over: false,
        }
    }
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.buf.len() as u64 + buf.len() as u64 > self.cap {
            self.over = true;
            return Err(io::Error::other("too large"));
        }
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
