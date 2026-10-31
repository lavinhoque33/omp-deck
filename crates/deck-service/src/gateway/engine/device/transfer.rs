//! A device's downloads and uploads (`docs/notes/mobile-pwa.md`, "Downloads are pull-based
//! JSON"): handles bound to one device session, at most [`api::MAX_DOWNLOADS`] downloads and
//! [`api::MAX_UPLOADS`] unfinished uploads open, each expiring [`api::TRANSFER_IDLE_SECS`] after
//! its last use. Chunks carry at most [`api::MAX_TRANSFER_CHUNK`] raw bytes.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_remote::api::{self, ApiError};

/// Export workers (a whole `omp --export` process each) in flight per device at most.
const MAX_EXPORT_WORKERS: usize = 1;
/// Blob workers (a transcript scan and an image decode each) in flight per device at most.
const MAX_BLOB_WORKERS: usize = 2;

/// Which worker is producing a download.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Export,
    Blob,
}

/// Finished uploads kept for `compose` at most (the oldest go first).
const MAX_FINISHED: usize = 16;

fn idle() -> Duration {
    Duration::from_secs(api::TRANSFER_IDLE_SECS)
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest {
        message: message.to_owned(),
    }
}

fn not_found(what: &str) -> ApiError {
    ApiError::NotFound {
        what: what.to_owned(),
    }
}

/// `prefix` + 16 random hex digits.
pub(super) fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // No OS randomness: the handle only has to be unique within this device session.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes = (n as u64).to_le_bytes();
    }
    let mut id = String::with_capacity(prefix.len() + 16);
    id.push_str(prefix);
    for b in bytes {
        id.push_str(&format!("{b:02x}"));
    }
    id
}

/// Where a download's bytes are.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    Bytes(Arc<Vec<u8>>),
    /// A file the gateway wrote (an export); read per chunk.
    File(PathBuf),
}

struct Download {
    source: Source,
    size: u64,
    used: Instant,
}

impl Source {
    /// Deletes the file behind an export; bytes need nothing.
    pub(super) fn discard(&self) {
        if let Source::File(path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One chunk to read off the engine thread.
pub(super) struct ChunkJob {
    source: Source,
    offset: u64,
    len: usize,
    size: u64,
    /// The last chunk: an export's file is deleted once it is read.
    last: bool,
}

impl ChunkJob {
    /// The chunk's bytes as base64 and whether it is the last one.
    pub(super) fn read(self) -> Result<api::Resp, ApiError> {
        let result = self.read_chunk();
        if self.last {
            self.source.discard();
        }
        result
    }

    fn read_chunk(&self) -> Result<api::Resp, ApiError> {
        use base64::Engine;
        let bytes = match &self.source {
            Source::Bytes(b) => {
                let start = usize::try_from(self.offset).unwrap_or(usize::MAX);
                b.get(start..start.saturating_add(self.len))
                    .ok_or_else(|| bad("offset beyond the end"))?
                    .to_vec()
            }
            Source::File(path) => {
                let failed = |e: std::io::Error| ApiError::Failed {
                    message: format!("reading the download failed: {}", e.kind()),
                };
                let mut f = std::fs::File::open(path).map_err(failed)?;
                f.seek(SeekFrom::Start(self.offset)).map_err(failed)?;
                let mut buf = Vec::with_capacity(self.len);
                f.take(self.len as u64)
                    .read_to_end(&mut buf)
                    .map_err(failed)?;
                if buf.len() != self.len {
                    return Err(ApiError::Failed {
                        message: "the download changed on disk".to_owned(),
                    });
                }
                buf
            }
        };
        Ok(api::Resp::Chunk {
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            eof: self.offset + bytes.len() as u64 >= self.size,
        })
    }
}

/// An upload's state.
enum Stage {
    /// Receiving: the bytes so far.
    Receiving(Vec<u8>),
    /// The last chunk arrived; the file is being stored.
    Storing,
    /// Stored at this path.
    Done(PathBuf),
}

struct Upload {
    name: String,
    size: u64,
    stage: Stage,
    used: Instant,
}

/// What an upload chunk did.
pub(super) enum ChunkOutcome {
    /// More bytes are expected.
    More,
    /// The file is complete: store these bytes as `name`, then call [`Transfers::stored`].
    Complete { name: String, bytes: Vec<u8> },
}

#[derive(Default)]
pub(super) struct Transfers {
    downloads: HashMap<String, Download>,
    uploads: HashMap<String, Upload>,
    /// Export and blob workers started and not yet answered; each holds a download slot.
    exporting: usize,
    blobbing: usize,
}

impl Drop for Transfers {
    /// The device session ended: its exports are not downloaded any more.
    fn drop(&mut self) {
        for d in self.downloads.values() {
            d.source.discard();
        }
    }
}

impl Transfers {
    /// Forgets what was not used for [`api::TRANSFER_IDLE_SECS`].
    pub(super) fn expire(&mut self, now: Instant) {
        let fresh = |used: Instant| now.saturating_duration_since(used) < idle();
        self.downloads.retain(|_, d| {
            let keep = fresh(d.used);
            if !keep {
                d.source.discard();
            }
            keep
        });
        // An upload being stored ends either way when its worker reports.
        self.uploads
            .retain(|_, u| matches!(u.stage, Stage::Storing) || fresh(u.used));
    }

    /// Reserves a download slot for a worker about to start: refused with `TooManyRequests`
    /// while the device's open downloads plus workers in flight fill [`api::MAX_DOWNLOADS`] or
    /// its workers of that kind are at their cap (1 export, 2 blobs). Every reservation ends
    /// in [`Transfers::fulfil`] or [`Transfers::release`].
    pub(super) fn reserve(&mut self, slot: Slot, now: Instant) -> Result<(), ApiError> {
        self.expire(now);
        let (count, cap) = match slot {
            Slot::Export => (&mut self.exporting, MAX_EXPORT_WORKERS),
            Slot::Blob => (&mut self.blobbing, MAX_BLOB_WORKERS),
        };
        if *count >= cap {
            return Err(ApiError::TooManyRequests {});
        }
        if self.downloads.len() + self.exporting + self.blobbing >= api::MAX_DOWNLOADS {
            return Err(ApiError::TooManyRequests {});
        }
        match slot {
            Slot::Export => self.exporting += 1,
            Slot::Blob => self.blobbing += 1,
        }
        Ok(())
    }

    /// A reserved worker ended without a download.
    pub(super) fn release(&mut self, slot: Slot) {
        let count = match slot {
            Slot::Export => &mut self.exporting,
            Slot::Blob => &mut self.blobbing,
        };
        *count = count.saturating_sub(1);
    }

    /// A reserved worker produced a download: its slot becomes the handle. The source is
    /// discarded if no handle can be made.
    pub(super) fn fulfil(
        &mut self,
        slot: Slot,
        source: Source,
        size: u64,
        now: Instant,
    ) -> Option<String> {
        self.release(slot);
        let id = self.add_download(source.clone(), size, now);
        if id.is_none() {
            source.discard();
        }
        id
    }

    /// A new download handle (`None`: [`api::MAX_DOWNLOADS`] are open).
    pub(super) fn add_download(
        &mut self,
        source: Source,
        size: u64,
        now: Instant,
    ) -> Option<String> {
        self.expire(now);
        if self.downloads.len() >= api::MAX_DOWNLOADS {
            return None;
        }
        let id = random_id("dl-");
        self.downloads.insert(
            id.clone(),
            Download {
                source,
                size,
                used: now,
            },
        );
        Some(id)
    }

    /// The chunk of `download` at `offset`. Past the end of the last chunk the handle is
    /// closed.
    pub(super) fn chunk(
        &mut self,
        download: &str,
        offset: u64,
        now: Instant,
    ) -> Result<ChunkJob, ApiError> {
        self.expire(now);
        let d = self
            .downloads
            .get_mut(download)
            .ok_or_else(|| not_found("download"))?;
        if offset > d.size {
            return Err(bad("offset beyond the end"));
        }
        d.used = now;
        let len = (d.size - offset).min(api::MAX_TRANSFER_CHUNK as u64) as usize;
        let job = ChunkJob {
            source: d.source.clone(),
            offset,
            len,
            size: d.size,
            last: offset + len as u64 >= d.size,
        };
        if job.last {
            self.downloads.remove(download);
        }
        Ok(job)
    }

    /// Starts an upload of `size` bytes named `name`.
    pub(super) fn begin_upload(
        &mut self,
        name: &str,
        size: u64,
        now: Instant,
    ) -> Result<String, ApiError> {
        self.expire(now);
        if size > api::MAX_UPLOAD_FILE {
            return Err(ApiError::TooLarge {});
        }
        if size == 0 {
            return Err(bad("an attachment cannot be empty"));
        }
        if name.trim().is_empty() || name.len() > 1024 {
            return Err(bad("an attachment needs a name"));
        }
        let open = self
            .uploads
            .values()
            .filter(|u| !matches!(u.stage, Stage::Done(_)))
            .count();
        if open >= api::MAX_UPLOADS {
            return Err(ApiError::TooManyRequests {});
        }
        let finished: Vec<(String, Instant)> = self
            .uploads
            .iter()
            .filter(|(_, u)| matches!(u.stage, Stage::Done(_)))
            .map(|(k, u)| (k.clone(), u.used))
            .collect();
        if finished.len() >= MAX_FINISHED
            && let Some((oldest, _)) = finished.iter().min_by_key(|(_, t)| *t)
        {
            self.uploads.remove(oldest);
        }
        let id = random_id("up-");
        self.uploads.insert(
            id.clone(),
            Upload {
                name: name.to_owned(),
                size,
                stage: Stage::Receiving(Vec::new()),
                used: now,
            },
        );
        Ok(id)
    }

    /// Appends `bytes` at `offset` (which must be where the upload stands).
    pub(super) fn upload_chunk(
        &mut self,
        upload: &str,
        offset: u64,
        bytes: &[u8],
        now: Instant,
    ) -> Result<ChunkOutcome, ApiError> {
        self.expire(now);
        let u = self
            .uploads
            .get_mut(upload)
            .ok_or_else(|| not_found("upload"))?;
        let Stage::Receiving(data) = &mut u.stage else {
            return Err(bad("the upload is complete"));
        };
        if offset != data.len() as u64 {
            return Err(bad("upload chunks must be contiguous"));
        }
        if bytes.is_empty() {
            return Err(bad("an empty upload chunk"));
        }
        if offset + bytes.len() as u64 > u.size {
            return Err(bad("more bytes than the upload's size"));
        }
        if data.is_empty() {
            data.reserve_exact(usize::try_from(u.size).unwrap_or(0));
        }
        data.extend_from_slice(bytes);
        u.used = now;
        if (data.len() as u64) < u.size {
            return Ok(ChunkOutcome::More);
        }
        let Stage::Receiving(bytes) = std::mem::replace(&mut u.stage, Stage::Storing) else {
            return Ok(ChunkOutcome::More);
        };
        Ok(ChunkOutcome::Complete {
            name: u.name.clone(),
            bytes,
        })
    }

    /// The complete upload was stored at `path` (`None`: storing failed, it is dropped).
    pub(super) fn stored(&mut self, upload: &str, path: Option<PathBuf>, now: Instant) {
        match path {
            Some(p) => {
                if let Some(u) = self.uploads.get_mut(upload) {
                    u.stage = Stage::Done(p);
                    u.used = now;
                }
            }
            None => {
                self.uploads.remove(upload);
            }
        }
    }

    pub(super) fn cancel_upload(&mut self, upload: &str) -> bool {
        self.uploads.remove(upload).is_some()
    }

    /// The stored files of finished uploads `ids`, in order.
    pub(super) fn attachments(
        &mut self,
        ids: &[String],
        now: Instant,
    ) -> Result<Vec<PathBuf>, ApiError> {
        self.expire(now);
        ids.iter()
            .map(|id| {
                let u = self
                    .uploads
                    .get_mut(id)
                    .ok_or_else(|| not_found("upload"))?;
                match &u.stage {
                    Stage::Done(p) => {
                        u.used = now;
                        Ok(p.clone())
                    }
                    _ => Err(bad("the upload is not finished")),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(resp: api::Resp) -> (Vec<u8>, bool) {
        use base64::Engine;
        match resp {
            api::Resp::Chunk { data, eof } => (
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
                eof,
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn downloads_are_chunked_bounded_and_expire() {
        let mut t = Transfers::default();
        let now = Instant::now();
        let size = api::MAX_TRANSFER_CHUNK + 10;
        let bytes: Vec<u8> = (0..size).map(|i| i as u8).collect();
        let id = t
            .add_download(Source::Bytes(Arc::new(bytes.clone())), size as u64, now)
            .unwrap();
        assert!(id.starts_with("dl-") && id.len() == 19);
        let (first, eof) = data(t.chunk(&id, 0, now).unwrap().read().unwrap());
        assert_eq!(first.len(), api::MAX_TRANSFER_CHUNK);
        assert!(!eof);
        assert!(matches!(
            t.chunk(&id, size as u64 + 1, now),
            Err(ApiError::BadRequest { .. })
        ));
        let (rest, eof) = data(
            t.chunk(&id, api::MAX_TRANSFER_CHUNK as u64, now)
                .unwrap()
                .read()
                .unwrap(),
        );
        assert!(eof);
        assert_eq!([first, rest].concat(), bytes);
        // The last chunk closed it.
        assert!(matches!(
            t.chunk(&id, 0, now),
            Err(ApiError::NotFound { .. })
        ));

        for _ in 0..api::MAX_DOWNLOADS {
            t.add_download(Source::Bytes(Arc::new(vec![1])), 1, now)
                .unwrap();
        }
        assert!(
            t.add_download(Source::Bytes(Arc::new(vec![1])), 1, now)
                .is_none()
        );
        let later = now + idle();
        assert!(
            t.add_download(Source::Bytes(Arc::new(vec![1])), 1, later)
                .is_some()
        );
    }

    #[test]
    fn a_file_download_reads_its_chunks_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.html");
        std::fs::write(&path, b"<html>hello</html>").unwrap();
        let mut t = Transfers::default();
        let now = Instant::now();
        let id = t.add_download(Source::File(path), 18, now).unwrap();
        let (bytes, eof) = data(t.chunk(&id, 6, now).unwrap().read().unwrap());
        assert_eq!(bytes, b"hello</html>");
        assert!(eof);
    }

    #[test]
    fn workers_reserve_download_slots_up_front() {
        let mut t = Transfers::default();
        let now = Instant::now();
        assert!(t.reserve(Slot::Export, now).is_ok());
        assert!(matches!(
            t.reserve(Slot::Export, now),
            Err(ApiError::TooManyRequests {})
        ));
        assert!(t.reserve(Slot::Blob, now).is_ok());
        assert!(t.reserve(Slot::Blob, now).is_ok());
        assert!(matches!(
            t.reserve(Slot::Blob, now),
            Err(ApiError::TooManyRequests {})
        ));
        // A finished worker frees its kind's slot, and a refused one holds nothing.
        t.release(Slot::Blob);
        assert!(t.reserve(Slot::Blob, now).is_ok());
        t.fulfil(Slot::Export, Source::Bytes(Arc::new(vec![1])), 1, now)
            .unwrap();
        assert!(t.reserve(Slot::Export, now).is_ok());
        t.release(Slot::Export);
        // Open downloads and reservations share MAX_DOWNLOADS.
        let mut t = Transfers::default();
        for _ in 0..api::MAX_DOWNLOADS - 2 {
            t.add_download(Source::Bytes(Arc::new(vec![1])), 1, now)
                .unwrap();
        }
        assert!(t.reserve(Slot::Blob, now).is_ok());
        assert!(t.reserve(Slot::Blob, now).is_ok());
        assert!(matches!(
            t.reserve(Slot::Export, now),
            Err(ApiError::TooManyRequests {})
        ));
    }

    #[test]
    fn an_export_file_is_deleted_when_its_download_ends_expires_or_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, b"<html>hello</html>").unwrap();
            p
        };
        let now = Instant::now();
        // The last chunk's read deletes it.
        let (done, expired, refused, dropped) = (file("a"), file("b"), file("c"), file("d"));
        let mut t = Transfers::default();
        let id = t.add_download(Source::File(done.clone()), 18, now).unwrap();
        let job = t.chunk(&id, 0, now).unwrap();
        assert!(done.exists());
        job.read().unwrap();
        assert!(!done.exists());
        // Idle expiry deletes it.
        t.add_download(Source::File(expired.clone()), 18, now)
            .unwrap();
        t.expire(now + idle());
        assert!(!expired.exists());
        // A refused download deletes it.
        t.reserve(Slot::Export, now).unwrap();
        for _ in 0..api::MAX_DOWNLOADS {
            t.add_download(Source::Bytes(Arc::new(vec![1])), 1, now)
                .unwrap();
        }
        assert!(
            t.fulfil(Slot::Export, Source::File(refused.clone()), 18, now)
                .is_none()
        );
        assert!(!refused.exists());
        // The device session ending deletes the rest.
        let mut t = Transfers::default();
        t.add_download(Source::File(dropped.clone()), 18, now)
            .unwrap();
        drop(t);
        assert!(!dropped.exists());
    }

    #[test]
    fn uploads_are_contiguous_sized_and_bounded() {
        let mut t = Transfers::default();
        let now = Instant::now();
        assert!(matches!(
            t.begin_upload("a.png", api::MAX_UPLOAD_FILE + 1, now),
            Err(ApiError::TooLarge {})
        ));
        assert!(t.begin_upload("a.png", 0, now).is_err());
        let id = t.begin_upload("a.png", 5, now).unwrap();
        assert!(matches!(
            t.upload_chunk(&id, 1, b"a", now),
            Err(ApiError::BadRequest { .. })
        ));
        assert!(matches!(
            t.upload_chunk(&id, 0, b"abc", now),
            Ok(ChunkOutcome::More)
        ));
        assert!(t.upload_chunk(&id, 3, b"def", now).is_err());
        assert!(t.attachments(std::slice::from_ref(&id), now).is_err());
        match t.upload_chunk(&id, 3, b"de", now).unwrap() {
            ChunkOutcome::Complete { name, bytes } => {
                assert_eq!(name, "a.png");
                assert_eq!(bytes, b"abcde");
            }
            ChunkOutcome::More => panic!("complete"),
        }
        t.stored(&id, Some(PathBuf::from("/x/a.png")), now);
        assert_eq!(
            t.attachments(std::slice::from_ref(&id), now).unwrap(),
            vec![PathBuf::from("/x/a.png")]
        );
        // At most MAX_UPLOADS unfinished at once; finished ones do not count.
        for _ in 0..api::MAX_UPLOADS {
            t.begin_upload("b", 1, now).unwrap();
        }
        assert!(matches!(
            t.begin_upload("b", 1, now),
            Err(ApiError::TooManyRequests {})
        ));
        // Idle ones expire.
        assert!(t.begin_upload("b", 1, now + idle()).is_ok());
        assert!(t.attachments(&[id], now + idle()).is_err());
    }
}
