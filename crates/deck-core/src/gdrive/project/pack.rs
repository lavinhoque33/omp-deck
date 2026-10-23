//! Builds the project archive, streaming into a file in the cache directory.
//!
//! Members: `omp/<rel>`, `sessions/<id>/session.jsonl`, `sessions/<id>/artifacts/<rel>`,
//! `blobs/<hex>` (shared sessions' blobs, once each), then `manifest.json` last. Every jsonl is
//! cut at its last newline (a running omp appends to it while we read), and the sha256 of each
//! member is computed while it streams into the archive.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use flate2::Compression;
use flate2::write::GzEncoder;
use memchr::memmem::Finder;
use sha2::{Digest, Sha256};

use super::snapshot::{FileRef, SessionSnap, Snapshot};
use super::{
    BlobManifest, FORMAT, ItemManifest, KIND, MAX_MEMBER, Manifest, Meter, OmpManifest, Progress,
    Pump, SessionsManifest, Stage, job_io,
};
use crate::gdrive::archive::{ManifestFile, hex};
use crate::gdrive::{Error, Result, rfc3339};

const BLOB_PREFIX: &[u8] = b"blob:sha256:";
/// `blob:sha256:` + 64 hex digits.
const BLOB_REF_LEN: usize = BLOB_PREFIX.len() + 64;
const READ_CHUNK: usize = 256 * 1024;

pub(super) struct Meta<'a> {
    pub unix: i64,
    pub host: &'a str,
    pub project: &'a Path,
    pub omp_version: &'a str,
}

pub(super) struct Packed {
    /// Size of the archive file.
    pub len: u64,
    pub omp_bytes: Option<u64>,
    pub sessions: Option<u64>,
    pub session_bytes: Option<u64>,
    /// Sessions left out, `session “<title>”: <reason>`.
    pub excluded: Vec<String>,
    /// Referenced blobs that were missing or damaged, with their state when checked.
    pub unresolved: BTreeMap<String, String>,
}

/// A session with the cut points found by the scan pass.
struct Planned<'a> {
    snap: &'a SessionSnap,
    /// Bytes of the session jsonl to archive (up to its last newline).
    cut: u64,
    /// Each artifact with the bytes of it to archive (`*.jsonl` cut like the session).
    artifacts: Vec<(&'a FileRef, u64)>,
    /// Referenced blobs that exist in the store (lowercase hex).
    blobs: Vec<String>,
}

/// Writes the archive of `snap` (its included parts) to `dest`.
pub(super) fn pack(
    snap: &Snapshot,
    dest: &Path,
    meta: &Meta<'_>,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<Packed> {
    let scan_err = job_io(cancel, "cannot read the sessions");
    let (planned, blob_sizes, left) = if snap.sessions_included() {
        plan_sessions(snap, cancel, progress).map_err(scan_err)?
    } else {
        (Vec::new(), BTreeMap::new(), Left::default())
    };
    let omp_files: &[FileRef] = if snap.omp_included() {
        &snap.omp_files
    } else {
        &[]
    };
    let total: u64 = omp_files.iter().map(|f| f.len).sum::<u64>()
        + planned
            .iter()
            .map(|p| p.cut + p.artifacts.iter().map(|(_, c)| c).sum::<u64>())
            .sum::<u64>()
        + blob_sizes.values().sum::<u64>();
    if omp_files.is_empty() && planned.is_empty() {
        return Err(Error::NothingToBackUp(meta.project.display().to_string()));
    }

    let mut pump = Pump::new(cancel, progress, Stage::Packing, total);
    let mut write = || -> io::Result<Packed> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dest)?;
        let gz = GzEncoder::new(
            BufWriter::with_capacity(256 * 1024, file),
            Compression::new(3),
        );
        let mut tar = tar::Builder::new(gz);
        let now = u64::try_from(meta.unix).unwrap_or(0);

        let mut omp_manifest = None;
        let mut omp_bytes = None;
        if !omp_files.is_empty() {
            let mut files = Vec::with_capacity(omp_files.len());
            for f in omp_files {
                files.push(append_file(
                    &mut tar,
                    &format!("omp/{}", f.rel),
                    f,
                    f.len,
                    now,
                    &mut pump,
                )?);
            }
            omp_bytes = Some(omp_files.iter().map(|f| f.len).sum());
            omp_manifest = Some(OmpManifest { files });
        }

        let mut sessions_manifest = None;
        let (mut sessions, mut session_bytes) = (None, None);
        if !planned.is_empty() {
            let mut items = Vec::with_capacity(planned.len());
            let mut bytes = 0u64;
            for p in &planned {
                let id = &p.snap.id;
                let main = append_file(
                    &mut tar,
                    &format!("sessions/{id}/session.jsonl"),
                    &p.snap.jsonl,
                    p.cut,
                    now,
                    &mut pump,
                )?;
                bytes += p.cut;
                let mut artifacts = Vec::with_capacity(p.artifacts.len());
                for (a, cut) in &p.artifacts {
                    artifacts.push(append_file(
                        &mut tar,
                        &format!("sessions/{id}/artifacts/{}", a.rel),
                        a,
                        *cut,
                        now,
                        &mut pump,
                    )?);
                    bytes += cut;
                }
                items.push(ItemManifest {
                    id: id.clone(),
                    file_name: p.snap.file_name.clone(),
                    title: p.snap.title.clone(),
                    modified_unix: p.snap.modified_unix,
                    size: main.size,
                    sha256: main.sha256,
                    artifacts,
                    blobs: p.blobs.clone(),
                });
            }
            let mut blobs = Vec::with_capacity(blob_sizes.len());
            for (name, size) in &blob_sizes {
                let blob = FileRef {
                    rel: name.clone(),
                    path: snap.blob_dir.join(name),
                    len: *size,
                    mtime_ns: 0,
                    mode: 0o644,
                };
                let entry = append_file(
                    &mut tar,
                    &format!("blobs/{name}"),
                    &blob,
                    *size,
                    now,
                    &mut pump,
                )?;
                if entry.sha256 != *name {
                    return Err(io::Error::other(format!(
                        "blob {name} changed while it was being backed up"
                    )));
                }
                blobs.push(BlobManifest {
                    sha256: entry.sha256,
                    size: entry.size,
                });
            }
            sessions = Some(items.len() as u64);
            session_bytes = Some(bytes);
            sessions_manifest = Some(SessionsManifest { items, blobs });
        }

        let name = meta
            .project
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let manifest = Manifest {
            format: FORMAT,
            kind: KIND.to_owned(),
            created: rfc3339(meta.unix),
            host: meta.host.to_owned(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            omp_version: meta.omp_version.to_owned(),
            project: meta.project.to_string_lossy().into_owned(),
            name,
            fingerprint: snap.fingerprint().to_owned(),
            omp: omp_manifest,
            sessions: sessions_manifest,
        };
        let json = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
        let mut header = member_header(json.len() as u64, 0o644, now);
        tar.append_data(&mut header, "manifest.json", &json[..])?;

        let gz = tar.into_inner()?;
        let file = gz
            .finish()?
            .into_inner()
            .map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        Ok(Packed {
            len: file.metadata()?.len(),
            omp_bytes,
            sessions,
            session_bytes,
            excluded: Vec::new(),
            unresolved: BTreeMap::new(),
        })
    };
    let packed = write().map_err(job_io(cancel, "cannot build the backup archive"));
    if packed.is_err() {
        let _ = fs::remove_file(dest);
    }
    let packed = packed?;
    pump.finish();
    Ok(Packed {
        excluded: left.excluded,
        unresolved: left.unresolved,
        ..packed
    })
}

fn member_header(size: u64, mode: u32, mtime: u64) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(size);
    h.set_mode(mode);
    h.set_mtime(mtime);
    h
}

/// Streams the first `size` bytes of `f` into the archive as `name`; returns its manifest entry.
/// A file that shrank since the snapshot is an error (the tar would be corrupt).
fn append_file<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    f: &FileRef,
    size: u64,
    mtime: u64,
    pump: &mut Pump<'_>,
) -> io::Result<ManifestFile> {
    let file = File::open(&f.path)?;
    let mut reader = Meter::new(file.take(size), pump, true);
    let mut header = member_header(size, f.mode, mtime);
    tar.append_data(&mut header, name, &mut reader)?;
    if reader.count != size {
        return Err(io::Error::other(format!(
            "{} changed while it was being backed up",
            f.path.display()
        )));
    }
    Ok(ManifestFile {
        path: f.rel.clone(),
        size,
        sha256: reader.digest(),
        mode: f.mode,
    })
}

// ---------------------------------------------------------------- scan pass

/// Cut points and blob references of every session; sessions that cannot be archived are left
/// out. Also the existing, verified blobs with their sizes.
fn plan_sessions<'a>(
    snap: &'a Snapshot,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> io::Result<(Vec<Planned<'a>>, BTreeMap<String, u64>, Left)> {
    let finder = Finder::new(BLOB_PREFIX);
    let mut pump = Pump::new(
        cancel,
        progress,
        Stage::Scanning,
        snap.sessions.len() as u64,
    );
    let mut left = Left::default();
    let mut planned = Vec::new();
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    let mut refs_of: Vec<BTreeSet<String>> = Vec::new();
    for s in &snap.sessions {
        let mut refs = BTreeSet::new();
        let cut = scan_jsonl(&s.jsonl.path, s.jsonl.len, &finder, &mut refs, cancel)?;
        if cut == 0 {
            left.exclude(s, "no complete line yet");
        } else if cut > MAX_MEMBER {
            left.exclude(
                s,
                "the session file is larger than 2 GiB (omp cannot restore it)",
            );
        } else {
            let mut artifacts = Vec::with_capacity(s.artifacts.len());
            let mut too_big = None;
            for a in &s.artifacts {
                let cut = if a.rel.ends_with(".jsonl") {
                    scan_jsonl(&a.path, a.len, &finder, &mut refs, cancel)?
                } else {
                    a.len
                };
                if cut > MAX_MEMBER {
                    too_big = Some(a.rel.as_str());
                }
                artifacts.push((a, cut));
            }
            if let Some(rel) = too_big {
                left.exclude(
                    s,
                    &format!("artifact {rel} is larger than 2 GiB (omp cannot restore it)"),
                );
            } else {
                wanted.extend(refs.iter().cloned());
                refs_of.push(refs);
                planned.push(Planned {
                    snap: s,
                    cut,
                    artifacts,
                    blobs: Vec::new(),
                });
            }
        }
        pump.item().map_err(|_| cancelled())?;
    }
    let mut blobs = BTreeMap::new();
    // State of every blob that failed verification, as seen right before it was read.
    let mut observed: BTreeMap<String, String> = BTreeMap::new();
    for hex in &wanted {
        let state = super::blob_state(&snap.blob_dir, hex);
        if let Some(size) = verified_blob_size(&snap.blob_dir.join(hex), hex, cancel)? {
            blobs.insert(hex.clone(), size);
        } else {
            observed.insert(hex.clone(), state);
        }
    }
    let mut kept = Vec::with_capacity(planned.len());
    for (mut p, refs) in planned.into_iter().zip(refs_of) {
        if let Some(big) = refs
            .iter()
            .find(|h| blobs.get(*h).is_some_and(|s| *s > MAX_MEMBER))
        {
            left.exclude(
                p.snap,
                &format!("blob {big} is larger than 2 GiB (omp cannot restore it)"),
            );
            continue;
        }
        for h in refs.iter().filter(|h| !blobs.contains_key(*h)) {
            log::warn!("blob {h} is missing or damaged; not backed up");
            let state = observed.get(h).cloned().unwrap_or_default();
            left.unresolved.insert(h.clone(), state);
        }
        p.blobs = refs.into_iter().filter(|h| blobs.contains_key(h)).collect();
        kept.push(p);
    }
    // Blobs only excluded sessions point to do not travel.
    let used: BTreeSet<&String> = kept.iter().flat_map(|p| &p.blobs).collect();
    blobs.retain(|name, _| used.contains(name));
    Ok((kept, blobs, left))
}

/// What the scan pass left out of the archive.
#[derive(Default)]
struct Left {
    /// `<session>: <reason>`.
    excluded: Vec<String>,
    /// Referenced blobs (hex) that are missing or damaged in the store, with their state as
    /// observed when they were checked (see `blob_state`).
    unresolved: BTreeMap<String, String>,
}

impl Left {
    fn exclude(&mut self, s: &SessionSnap, why: &str) {
        let label: String = if s.title.is_empty() { &s.id } else { &s.title }
            .chars()
            .take(60)
            .collect();
        log::warn!("session {} not backed up: {why}", s.id);
        self.excluded.push(format!("session “{label}”: {why}"));
    }
}

fn cancelled() -> io::Error {
    super::cancelled_io()
}

/// The size of blob file `path` if it exists and its sha256 equals `hex`.
fn verified_blob_size(path: &Path, hex_name: &str, cancel: &AtomicBool) -> io::Result<Option<u64>> {
    let mut file = match File::open(path) {
        Ok(f) if f.metadata()?.is_file() => f,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut size = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex(&hasher.finalize()) == hex_name).then_some(size))
}

/// Reads the first `limit` bytes of a jsonl: collects blob references and returns the offset
/// just past the last newline (0 when there is none).
fn scan_jsonl(
    path: &Path,
    limit: u64,
    finder: &Finder<'_>,
    refs: &mut BTreeSet<String>,
    cancel: &AtomicBool,
) -> io::Result<u64> {
    let mut file = File::open(path)?.take(limit);
    let mut buf = vec![0u8; READ_CHUNK];
    let mut scanner = BlobScanner::new(finder);
    let (mut pos, mut cut) = (0u64, 0u64);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(i) = memchr::memrchr(b'\n', &buf[..n]) {
            cut = pos + i as u64 + 1;
        }
        pos += n as u64;
        scanner.feed(&buf[..n], refs);
    }
    scanner.finish(refs);
    Ok(cut)
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `\bblob:sha256:([a-f0-9]{64})\b` over a byte stream delivered in chunks. The last
/// [`BLOB_REF_LEN`]` + 1` bytes are carried over so references (and their boundary characters)
/// that straddle chunks are found.
struct BlobScanner<'f, 'n> {
    finder: &'f Finder<'n>,
    carry: Vec<u8>,
    window: Vec<u8>,
    /// Stream bytes fed so far.
    fed: u64,
}

impl<'f, 'n> BlobScanner<'f, 'n> {
    fn new(finder: &'f Finder<'n>) -> Self {
        BlobScanner {
            finder,
            carry: Vec::new(),
            window: Vec::new(),
            fed: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8], out: &mut BTreeSet<String>) {
        self.window.clear();
        self.window.extend_from_slice(&self.carry);
        self.window.extend_from_slice(chunk);
        let origin = self.fed - self.carry.len() as u64;
        self.fed += chunk.len() as u64;
        self.scan(origin, false, out);
        let keep = self.window.len().min(BLOB_REF_LEN + 1);
        self.carry.clear();
        self.carry
            .extend_from_slice(&self.window[self.window.len() - keep..]);
    }

    /// The stream ended: a reference at its very end is complete.
    fn finish(&mut self, out: &mut BTreeSet<String>) {
        self.window.clear();
        self.window.extend_from_slice(&self.carry);
        let origin = self.fed - self.carry.len() as u64;
        self.scan(origin, true, out);
    }

    /// `origin` = number of stream bytes before `window[0]`.
    fn scan(&self, origin: u64, last: bool, out: &mut BTreeSet<String>) {
        let buf = &self.window[..];
        for at in self.finder.find_iter(buf) {
            // A match at the window start with earlier bytes unseen was judged in an earlier
            // window, where its predecessor was visible.
            if at == 0 && origin != 0 {
                continue;
            }
            if at > 0 && is_word(buf[at - 1]) {
                continue;
            }
            let digits = at + BLOB_PREFIX.len();
            let end = digits + 64;
            let Some(hex) = buf.get(digits..end) else {
                continue;
            };
            if !hex.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                continue;
            }
            match buf.get(end) {
                Some(&b) if is_word(b) => continue,
                None if !last => continue,
                _ => {}
            }
            // The digits are ASCII hex, so this is valid UTF-8.
            out.insert(String::from_utf8_lossy(hex).into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(chunks: &[&[u8]]) -> Vec<String> {
        let finder = Finder::new(BLOB_PREFIX);
        let mut scanner = BlobScanner::new(&finder);
        let mut out = BTreeSet::new();
        for c in chunks {
            scanner.feed(c, &mut out);
        }
        scanner.finish(&mut out);
        out.into_iter().collect()
    }

    fn reference(c: char) -> String {
        format!("blob:sha256:{}", c.to_string().repeat(64))
    }

    #[test]
    fn references_are_found_across_chunk_boundaries_and_at_the_end() {
        let r = reference('a');
        let text = format!("x\"{r}\" and {}", reference('b'));
        let bytes = text.as_bytes();
        let want = vec!['a'.to_string().repeat(64), 'b'.to_string().repeat(64)];
        assert_eq!(found(&[bytes]), want);
        // Every split point, including inside the prefix, the digits and before the end.
        for cut in 1..bytes.len() {
            assert_eq!(found(&[&bytes[..cut], &bytes[cut..]]), want, "cut {cut}");
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(found(&singles), want);
    }

    #[test]
    fn word_boundaries_and_digit_rules_are_enforced() {
        let a = 'a'.to_string().repeat(64);
        for bad in [
            format!("xblob:sha256:{a}"),
            format!("_blob:sha256:{a}"),
            format!("blob:sha256:{a}f"),
            format!("blob:sha256:{a}_"),
            format!("blob:sha256:{}A", &a[..63]),
            format!("blob:sha256:{}", &a[..63]),
        ] {
            assert!(found(&[bad.as_bytes()]).is_empty(), "{bad}");
        }
        for good in [
            format!("blob:sha256:{a}"),
            format!("\"blob:sha256:{a}\""),
            format!("-blob:sha256:{a}."),
        ] {
            assert_eq!(found(&[good.as_bytes()]), [a.as_str()], "{good}");
        }
    }
}
