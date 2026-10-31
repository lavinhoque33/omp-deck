//! Files a remote client attaches to a prompt: written owner-only below
//! `<data dir>/omp-deck/attachments/<owner>/<sha256-16hex>-<sanitized name>`, then pasted into
//! omp as their path (like a file dropped on the terminal), so they stay where omp's session
//! refers to them. Per file at most [`MAX_FILE`] bytes, per owner at most [`MAX_OWNER`]: over
//! quota the owner's files older than [`EVICT_AFTER`] go first (oldest first), otherwise the
//! write is refused. Revoking an owner removes its directory ([`AttachmentStore::remove_owner`]).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

/// Largest single attachment.
pub const MAX_FILE: u64 = 20 * 1024 * 1024;
/// Largest total of one owner's attachments.
pub const MAX_OWNER: u64 = 200 * 1024 * 1024;
/// Files at least this old may be evicted to make room.
pub const EVICT_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Longest sanitized file name part (bytes).
const NAME_MAX: usize = 80;

#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// The file alone, or the owner's files with it, exceed the limits (nothing old enough to
    /// evict).
    #[error("the attachment is too large")]
    TooLarge,
    #[error("not a valid attachment owner")]
    BadOwner,
    /// The owner was revoked while the file was being written: its directory is gone again.
    #[error("the device was revoked")]
    Revoked,
    #[error("no per-user data directory")]
    NoDataDir,
    #[error("storing the attachment failed: {0}")]
    Io(#[from] io::Error),
}

/// Which owners have attachment work in flight, and which of those were revoked meanwhile. An
/// owner is tracked only while a [`Writer`] or a revocation [`Cleanup`] of it lives: a
/// revocation marks it so writers racing the removal clean up after themselves, and the mark
/// goes with the last of them (the set stays as small as the work in flight).
#[derive(Debug, Default)]
pub struct Revocations(std::sync::Mutex<std::collections::HashMap<String, Gate>>);

#[derive(Debug, Default)]
struct Gate {
    writers: usize,
    cleanups: usize,
    revoked: bool,
}

impl Revocations {
    fn gates(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Gate>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A store for `owner` begins; refused while a revocation of it is still being carried
    /// out (its writers or the directory removal run). Take it before the owner can be revoked
    /// (on the thread that revokes), then hand it to the worker that writes.
    pub fn writer(self: &Arc<Self>, owner: &str) -> Result<Writer, AttachError> {
        let mut gates = self.gates();
        let gate = gates.entry(owner.to_owned()).or_default();
        if gate.revoked {
            return Err(AttachError::Revoked);
        }
        gate.writers += 1;
        Ok(Writer {
            gates: self.clone(),
            owner: owner.to_owned(),
        })
    }

    /// `owner` is revoked: writers in flight remove what they write. Call before removing its
    /// directory and keep the [`Cleanup`] until that removal is done.
    pub fn revoke(self: &Arc<Self>, owner: &str) -> Cleanup {
        let mut gates = self.gates();
        let gate = gates.entry(owner.to_owned()).or_default();
        gate.revoked = true;
        gate.cleanups += 1;
        Cleanup {
            gates: self.clone(),
            owner: owner.to_owned(),
        }
    }

    /// Owners tracked now (their writers or revocation cleanup still run).
    pub fn tracked(&self) -> usize {
        self.gates().len()
    }

    fn is_revoked(&self, owner: &str) -> bool {
        self.gates().get(owner).is_some_and(|g| g.revoked)
    }

    fn release(&self, owner: &str, f: impl FnOnce(&mut Gate)) {
        let mut gates = self.gates();
        if let Some(gate) = gates.get_mut(owner) {
            f(gate);
            if gate.writers == 0 && gate.cleanups == 0 {
                gates.remove(owner);
            }
        }
    }
}

/// One store in flight for an owner ([`Revocations::writer`]).
#[derive(Debug)]
pub struct Writer {
    gates: Arc<Revocations>,
    owner: String,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.gates
            .release(&self.owner, |g| g.writers = g.writers.saturating_sub(1));
    }
}

/// A revocation's directory removal in flight ([`Revocations::revoke`]).
#[derive(Debug)]
pub struct Cleanup {
    gates: Arc<Revocations>,
    owner: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.gates
            .release(&self.owner, |g| g.cleanups = g.cleanups.saturating_sub(1));
    }
}

/// The attachment directories of every owner.
#[derive(Clone, Debug)]
pub struct AttachmentStore {
    root: PathBuf,
}

impl AttachmentStore {
    /// `<data dir>/omp-deck/attachments`.
    pub fn open() -> Result<AttachmentStore, AttachError> {
        let data = dirs::data_dir().ok_or(AttachError::NoDataDir)?;
        Ok(AttachmentStore::at(
            data.join("omp-deck").join("attachments"),
        ))
    }

    pub fn at(root: PathBuf) -> AttachmentStore {
        AttachmentStore { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn owner_dir(&self, owner: &str) -> Result<PathBuf, AttachError> {
        let valid = !owner.is_empty()
            && owner.len() <= 64
            && owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid {
            return Err(AttachError::BadOwner);
        }
        Ok(self.root.join(owner))
    }

    /// Stores `bytes` as `name` for `owner` and returns the file's absolute path. The same bytes
    /// under the same name are stored once (the existing file is reused and counts as new).
    pub fn store(&self, owner: &str, name: &str, bytes: &[u8]) -> Result<PathBuf, AttachError> {
        let len = bytes.len() as u64;
        if len > MAX_FILE {
            return Err(AttachError::TooLarge);
        }
        let dir = self.owner_dir(owner)?;
        let digest = Sha256::digest(bytes);
        let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("{hash}-{}", sanitize_name(name)));
        crate::gateway::private_dir(&dir)?;
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == len) {
            std::fs::File::options()
                .write(true)
                .open(&path)?
                .set_modified(SystemTime::now())?;
            return Ok(path);
        }
        make_room(&dir, len, SystemTime::now())?;
        crate::gateway::write_private(&path, bytes)?;
        Ok(path)
    }

    /// [`store`](Self::store) for the owner of `writer`, which may be revoked meanwhile: a
    /// write that lands after (or races) the revocation removes the owner's directory again and
    /// fails, so a revoked device's directory is never left behind. [`Revocations::revoke`]
    /// comes first on revocation, then the removal; this checks after writing. The writer
    /// stays counted until it is dropped, after this cleanup.
    pub fn store_for(
        &self,
        writer: &Writer,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, AttachError> {
        let owner = writer.owner.as_str();
        if writer.gates.is_revoked(owner) {
            return Err(AttachError::Revoked);
        }
        let path = self.store(owner, name, bytes);
        if writer.gates.is_revoked(owner) {
            self.remove_owner(owner)?;
            return Err(AttachError::Revoked);
        }
        path
    }

    /// Bytes stored for `owner`.
    pub fn usage(&self, owner: &str) -> Result<u64, AttachError> {
        let dir = self.owner_dir(owner)?;
        Ok(files(&dir)?.iter().map(|f| f.1).sum())
    }

    /// Deletes every attachment of `owner` (a revoked device). A missing directory is fine.
    pub fn remove_owner(&self, owner: &str) -> Result<(), AttachError> {
        let dir = self.owner_dir(owner)?;
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// `(path, size, modified)` of the regular files directly in `dir` (none when it is missing).
fn files(dir: &Path) -> io::Result<Vec<(PathBuf, u64, SystemTime)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for e in entries {
        let e = e?;
        let m = e.metadata()?;
        if m.is_file() {
            out.push((
                e.path(),
                m.len(),
                m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            ));
        }
    }
    Ok(out)
}

/// Evicts the oldest files older than [`EVICT_AFTER`] until `incoming` more bytes fit below
/// [`MAX_OWNER`]; refuses (evicting nothing) when even that is not enough.
fn make_room(dir: &Path, incoming: u64, now: SystemTime) -> Result<(), AttachError> {
    let mut all = files(dir)?;
    let mut total: u64 = all.iter().map(|f| f.1).sum();
    if total + incoming <= MAX_OWNER {
        return Ok(());
    }
    all.sort_by_key(|f| f.2);
    let old: Vec<&(PathBuf, u64, SystemTime)> = all
        .iter()
        .filter(|f| now.duration_since(f.2).is_ok_and(|age| age >= EVICT_AFTER))
        .collect();
    let freeable: u64 = old.iter().map(|f| f.1).sum();
    if total - freeable + incoming > MAX_OWNER {
        return Err(AttachError::TooLarge);
    }
    for (path, size, _) in old {
        if total + incoming <= MAX_OWNER {
            break;
        }
        std::fs::remove_file(path)?;
        total -= size;
    }
    Ok(())
}

/// A file name safe on every platform: the last path component, letters, digits, `.`, `-`, `_`
/// kept, anything else `_`, no leading dot, at most [`NAME_MAX`] bytes, never empty.
pub fn sanitize_name(name: &str) -> String {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out = String::with_capacity(leaf.len().min(NAME_MAX));
    for c in leaf.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
            c
        } else {
            '_'
        };
        if out.len() + c.len_utf8() > NAME_MAX {
            break;
        }
        out.push(c);
    }
    let trimmed = out.trim_start_matches('.');
    if trimmed.is_empty() {
        "file".to_owned()
    } else {
        trimmed.to_owned()
    }
}
